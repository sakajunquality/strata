//! How fast objects are hashed here, against the `sha1` crate:
//!
//!     cargo run --release -p strata-git --example sha1_speed

use std::time::Instant;

use sha1::Digest;

fn main() {
    let data = vec![0x5au8; 256 << 20];
    let started = Instant::now();
    let ours = strata_git::sha1::Sha1::digest(&data);
    let ours_time = started.elapsed();
    let started = Instant::now();
    let reference: [u8; 20] = sha1::Sha1::digest(&data).into();
    let reference_time = started.elapsed();
    assert_eq!(ours, reference);
    let speed = |time: std::time::Duration| 256.0 / time.as_secs_f64();
    println!("strata: {:.0} MiB/s, sha1 crate: {:.0} MiB/s", speed(ours_time), speed(reference_time));
}
