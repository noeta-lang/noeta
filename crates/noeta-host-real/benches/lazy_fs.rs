//! P-LAZY bench: time-to-first-line on a large file, the old whole-file snapshot vs the new lazy
//! stream. The snapshot arm reads and allocates the entire file before it can hand back the first
//! line; the lazy arm pulls only the first buffered chunk. Over a multi-megabyte file the gap is the
//! win P-LAZY buys — a workload that opens a large file and reads incrementally no longer pays for
//! the whole file up front. A fresh `RealHost` is built per iteration in *both* arms (so the tokio
//! runtime setup is a shared constant), making the snapshot-vs-lazy delta the read strategy alone.

use criterion::{Criterion, criterion_group, criterion_main};
use noeta_host_real::RealHost;
use noeta_stdlib::{FileHandle, FileReader, FileSystem, ReadSource};
use std::hint::black_box;

/// Write a ~8 MB, 200k-line fixture once and return its directory guard and its path.
///
/// The guard is handed back rather than dropped here: the path used to be one fixed name under the
/// system temp dir, so two concurrent bench runs (two checkouts, or a bench beside the test suite)
/// wrote and read the same 8 MB file, and a timing measured against a file another process was
/// rewriting is not a measurement. The caller holds the guard for the length of the bench.
fn write_fixture() -> (noeta_test_temp::TempDir, String) {
    let dir = noeta_test_temp::TempDir::new("host-lazy-fs-bench");
    let path = dir.join("lazy_fs.txt").to_string_lossy().into_owned();
    let mut content = String::with_capacity(8 * 1024 * 1024);
    for i in 0..200_000 {
        content.push_str(&format!(
            "line number {i} with some padding to make each line wider\n"
        ));
    }
    std::fs::write(&path, &content).unwrap();
    (dir, path)
}

fn bench_first_line(c: &mut Criterion) {
    // `_fixture` is named (not `_`), so the fixture outlives the benchmarks rather than being
    // deleted on the spot.
    let (_fixture, path) = write_fixture();
    let mut group = c.benchmark_group("fs_open_first_line");

    // The pre-P-LAZY behavior: snapshot the whole file, then take the first line.
    group.bench_function("snapshot", |b| {
        b.iter(|| {
            let host = RealHost::new().unwrap();
            let content = host.fs_read(black_box(&path)).unwrap();
            black_box(content.lines().next().unwrap().to_string());
        });
    });

    // P-LAZY: stream — open and pull only the first line.
    group.bench_function("lazy", |b| {
        b.iter(|| {
            let mut host = RealHost::new().unwrap();
            let source = host.fs_open_read(black_box(&path)).unwrap();
            debug_assert!(matches!(source, ReadSource::Lazy(_)));
            let mut handle = FileHandle::open_read(&path, source);
            black_box(handle.read_line(&mut host).unwrap().unwrap());
        });
    });

    group.finish();
    // The fixture goes with `_fixture`'s guard.
}

criterion_group!(benches, bench_first_line);
criterion_main!(benches);
