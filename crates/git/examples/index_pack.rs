//! Times indexing a packfile, the work a push costs the server, and shows
//! the memory it took beyond the pack itself:
//!
//!     cargo run --release -p strata-git --example index_pack -- path/to/pack-*.pack

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use strata_git::pack::index_pack;
use strata_git::PackSet;

/// Counts what is allocated, to report the most held at any one time.
struct Counting;

static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let held = HELD.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(held, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(pointer, layout)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() {
    let path = std::env::args().nth(1).expect("usage: index_pack <packfile>");
    let data = Bytes::from(std::fs::read(&path).expect("readable packfile"));
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let started = Instant::now();
    let indexed = index_pack(&data, &PackSet::new(Vec::new())).expect("a valid pack");
    let peak = PEAK.load(Ordering::Relaxed) - before;
    println!(
        "{} objects, {} commits, {} MiB in {:.2?}; at most {} MiB allocated on top, {} bytes an object",
        indexed.index.len(),
        indexed.graph.len(),
        data.len() >> 20,
        started.elapsed(),
        peak >> 20,
        peak / indexed.index.len().max(1)
    );
}
