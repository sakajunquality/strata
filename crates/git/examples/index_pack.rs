//! Times indexing a packfile, the work a push costs the server:
//!
//!     cargo run --release -p strata-git --example index_pack -- path/to/pack-*.pack

use std::time::Instant;

use bytes::Bytes;
use strata_git::pack::index_pack;
use strata_git::PackSet;

fn main() {
    let path = std::env::args().nth(1).expect("usage: index_pack <packfile>");
    let data = Bytes::from(std::fs::read(&path).expect("readable packfile"));
    let started = Instant::now();
    let indexed = index_pack(&data, &PackSet::new(Vec::new())).expect("a valid pack");
    println!("{} objects, {} commits, {} MiB in {:.2?}", indexed.index.len(), indexed.graph.len(), data.len() >> 20, started.elapsed());
}
