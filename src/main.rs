//! rcp —— 带进度条与传输速率的 `cp`。

mod cli;
mod copier;
mod meta;
mod progress;
mod scan;

use std::time::Instant;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let cfg = cli::config_from_env();
    let mode = progress::mode(cfg.progress, cfg.quiet);

    // ---------- 1. 扫描：确定条目与总字节数（进度条总量依赖它） ----------
    // scan_pb 在这个块里创建/销毁：它的 steady tick 线程必须在复制开始前结束
    let plan = {
        let scan_pb = progress::ScanProgress::new(mode);
        scan::Scanner::new(&cfg, &scan_pb).run()
    };

    // ---------- 2. 复制 ----------
    let mut pb = progress::Progress::new(mode, plan.total_bytes);
    let mut copier = copier::Copier::new(&cfg, &pb);

    let started = Instant::now();
    let total_items = plan.items.len() as u64;
    for (i, item) in plan.items.iter().enumerate() {
        pb.set_current(i as u64 + 1, total_items, &item.src);
        copier.handle(item);
    }

    // ---------- 3. 目录属性回写（自底向上，子项先于父目录） ----------
    for item in plan.items.iter().rev() {
        copier.finalize_dir(item);
    }
    let elapsed = started.elapsed();

    let st = copier.stats;
    drop(copier);
    let secs = elapsed.as_secs_f64().max(1e-9);
    let summary = format!(
        "{} in {:.2}s ({}), {} files, {} dirs, {} links, {} special, {} skipped, {} failed",
        progress::human_bytes(st.bytes),
        elapsed.as_secs_f64(),
        progress::human_rate(st.bytes as f64 / secs),
        st.files,
        st.dirs,
        st.links,
        st.specials,
        st.skipped,
        st.failed
    );
    pb.finish(if pb.is_on() { Some(&summary) } else { None });

    // 与 GNU cp 一致：有任何条目失败（含 -i 时用户回答 no）就返回 1
    if plan.errors > 0 || st.failed > 0 || st.declined > 0 {
        1
    } else {
        0
    }
}
