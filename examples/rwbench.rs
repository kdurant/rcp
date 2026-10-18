//! 开发用微基准：把 read/write 回退路径单独拎出来测吞吐，用来给 `copier::RW_CHUNK` 定值。
//!
//! 用法: cargo run --release --example rwbench -- <src> <dst> [chunk]
//!
//! 实测参考（/dev/shm → /tmp，1 GiB，tmpfs→tmpfs）：
//!   chunk=32K/64K ≈ 0.40s，chunk≥128K ≈ 0.58s（内核大 folio 拷贝路径劣化）

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (src, dst) = (&args[1], &args[2]);
    let chunk: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(64 * 1024);

    let t0 = Instant::now();
    let fin = File::open(src).unwrap();
    let fout = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(dst)
        .unwrap();
    let mut buf = vec![0u8; chunk];
    let mut r: &File = &fin;
    let mut w: &File = &fout;
    let mut total: u64 = 0;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => panic!("{e}"),
        };
        w.write_all(&buf[..n]).unwrap();
        total += n as u64;
    }
    let el = t0.elapsed().as_secs_f64();
    eprintln!(
        "rwbench: chunk={} {} bytes in {:.3}s ({:.2} GiB/s)",
        chunk,
        total,
        el,
        (total as f64 / el) / (1024.0 * 1024.0 * 1024.0)
    );
}
