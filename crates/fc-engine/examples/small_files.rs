//! Time copying many small files through the engine.
//!
//!     cargo run --release -p fc-engine --example small_files -- [count] [size] [runs] [workers] [--verify] [--os-copy]
//!
//! Defaults: 20000 files of 128 bytes, 3 runs, the engine's default worker count.
//! Prints each run and the median. The source tree is made once; every run copies
//! into a fresh destination, so runs are comparable.
//!
//! `--os-copy` copies each planned file with `std::fs::copy` instead of the
//! engine (on Windows that is `CopyFileExW`), as a floor for what the OS costs.

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fc_engine::{AutoHandler, CopyOptions, Copier, JobControl, Progress, RetryPolicy, VerifyMode, plan_copy};

fn main() {
    let verify = std::env::args().any(|a| a == "--verify");
    let os_copy = std::env::args().any(|a| a == "--os-copy");
    let mut args = std::env::args().skip(1).filter(|a| a != "--verify" && a != "--os-copy");
    let count: usize = args.next().map(|v| v.parse().expect("count")).unwrap_or(20_000);
    let size: usize = args.next().map(|v| v.parse().expect("size")).unwrap_or(128);
    let runs: usize = args.next().map(|v| v.parse().expect("runs")).unwrap_or(3);
    let workers: Option<usize> = args.next().map(|v| v.parse().expect("workers"));

    let base = tempfile::tempdir().expect("temp dir");
    let src = base.path().join("src");
    let made = Instant::now();
    for i in 0..count {
        // 1000 files per folder, like a photo library or a source tree.
        let dir = src.join(format!("d{:03}", i / 1000));
        if i % 1000 == 0 {
            fs::create_dir_all(&dir).expect("make folder");
        }
        fs::write(dir.join(format!("f{i:06}.bin")), vec![(i % 251) as u8; size]).expect("write source");
    }
    println!("made {count} files of {size} B in {:.1} s", made.elapsed().as_secs_f64());

    let mut times = Vec::new();
    for run in 0..runs {
        let dst = base.path().join(format!("dst{run}"));
        fs::create_dir_all(&dst).expect("make destination");
        let mut options = CopyOptions { verify: if verify { VerifyMode::ReadBack } else { VerifyMode::Off }, ..Default::default() };
        if let Some(w) = workers {
            options.small_file_workers = w;
        }
        let copier = Copier::new(options, Arc::new(AutoHandler::new(RetryPolicy::default())));
        let plan = plan_copy(std::slice::from_ref(&src), &dst).expect("plan");
        let started = Instant::now();
        if os_copy {
            for dir in &plan.dirs {
                fs::create_dir_all(dir).expect("make folder");
            }
            for item in &plan.items {
                fs::copy(&item.src, &item.dst).expect("os copy");
            }
        } else {
            let report = copier.run_plan(&plan, &dst, &Progress::new(), &JobControl::new()).expect("run");
            assert!(report.is_clean(), "failed: {:?}", report.failed.first());
            assert_eq!(report.copied.len(), count);
        }
        let took = started.elapsed();
        println!("run {}: {:.2} s, {:.0} files/s", run + 1, took.as_secs_f64(), count as f64 / took.as_secs_f64());
        times.push(took);
        let _ = fs::remove_dir_all(&dst);
    }
    times.sort();
    let median: Duration = times[times.len() / 2];
    let mode = if os_copy { "std::fs::copy".to_string() } else { format!("{} workers", workers.map_or("default".to_string(), |w| w.to_string())) };
    println!("median: {:.2} s ({mode})", median.as_secs_f64());
}
