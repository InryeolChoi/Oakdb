//! Power-cut experiment (docs/experiments/0008-power-cuts.md, issue #21).
//!
//! ```sh
//! cargo run --example crash_sim -- one 1234          # replay one seed
//! cargo run --example crash_sim -- sweep 0 10000     # seeds 0..10000
//! cargo run --example crash_sim -- every 7 8         # workload seed 7: every
//!                                                     # cut point, crash seeds 0..8
//! ```

mod harness;

use std::collections::BTreeMap;

use harness::{run, Case, Run, Verdict, Workload};
use oakdb::fs::sim::Landing;

const PAGE: u64 = 4096;

/// How many seeds ended one way, and the shortest of them.
#[derive(Default)]
struct Count {
    seeds: u64,
    shortest: Option<Shortest>,
}

/// The seed that broke the fewest operations into the run.
struct Shortest {
    seed: u64,
    cut_after: u64,
}

/// The verdict, and for a failed open, when the power went: while
/// creating the database or during a commit.
fn label(run: &Run) -> &'static str {
    let creating = run.cut_commit.is_none() && run.acked_commits == 0;
    match run.verdict {
        Verdict::Intact { .. } => "intact",
        Verdict::OpenFailed(_) if creating => "open failed (cut while creating)",
        Verdict::OpenFailed(_) => "open failed (cut in a commit)",
        Verdict::ReadFailed(_) => "read failed",
        Verdict::Panicked(_) => "panicked on reopening",
        Verdict::LostAcknowledged(_) => "lost an acknowledged commit",
        Verdict::WrongValue(_) => "wrong value",
        Verdict::HalfCommit(_) => "half a commit",
    }
}

/// What became of one unsynced write: `#` a sector that landed, `.` one
/// that did not.
fn landing(landing: &Landing) -> String {
    match landing {
        Landing::Lost => "lost".into(),
        Landing::Kept => "kept".into(),
        Landing::Torn { sectors } => {
            let sectors: String = sectors.iter().map(|&s| if s { '#' } else { '.' }).collect();
            format!("torn, sectors {sectors}")
        }
    }
}

/// A panic the engine raises on reopening is counted, not printed every
/// time; one the harness raises still stops the run with its message.
fn quiet_engine_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info
            .location()
            .is_some_and(|at| at.file().contains("crash_sim"))
        {
            default(info);
        }
    }));
}

fn describe(run: &Run) {
    println!("acknowledged commits: {}", run.acked_commits);
    match run.cut_commit {
        Some(i) => println!("power cut during commit {i}"),
        None => println!("power cut while opening, or not at all"),
    }
    for path in &run.report.vanished {
        println!("vanished: {}", path.display());
    }
    println!("unsynced writes at the crash:");
    for w in &run.report.writes {
        let first = w.offset / PAGE;
        let pages = (w.len as u64).div_ceil(PAGE);
        let what = landing(&w.landing);
        let at = if pages == 1 {
            format!("page {first}")
        } else {
            format!("pages {first}..{}", first + pages)
        };
        println!("  {at}: {what}");
    }
    println!("verdict: {:?}", run.verdict);
    let keys = match &run.verdict {
        Verdict::LostAcknowledged(k) | Verdict::WrongValue(k) | Verdict::HalfCommit(k) => k,
        _ => return,
    };
    let short = |v: Option<&Vec<u8>>| match v {
        None => "absent".to_string(),
        Some(v) => format!("{} bytes", v.len()),
    };
    let after = run.after.as_ref().unwrap();
    let cut = run.cut_state.as_ref().unwrap_or(&run.acked_state);
    println!("  key: acknowledged / cut commit / after the crash");
    for k in keys {
        let (old, new, now) = (run.acked_state.get(k), cut.get(k), after.get(k));
        let mark = if now == old {
            "old"
        } else if now == new {
            "NEW"
        } else {
            "???"
        };
        println!(
            "  {k:>4}: {} / {} / {} ({mark})",
            short(old),
            short(new),
            short(now)
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize| -> u64 { args[i].parse().expect("a number") };
    match args.first().map(String::as_str) {
        Some("one") => {
            let case = Case::from_seed(num(1));
            let total = case.workload.total_ops();
            println!(
                "seed {}: {} commits, {total} disk operations, power cut after {}, crash seed {}",
                case.seed,
                case.workload.commits.len(),
                case.cut_after,
                case.crash_seed
            );
            describe(&case.run());
        }
        Some("sweep") => {
            let (from, count) = (num(1), num(2));
            let mut tally: BTreeMap<&str, Count> = BTreeMap::new();
            let mut errors: BTreeMap<String, u64> = BTreeMap::new();
            quiet_engine_panics();
            for seed in from..from + count {
                let case = Case::from_seed(seed);
                let run = case.run();
                if let Verdict::OpenFailed(e) | Verdict::ReadFailed(e) | Verdict::Panicked(e) = &run.verdict {
                    // "page 9 has ..." and "page 30 has ..." count together.
                    let general: String = e.chars().filter(|c| !c.is_ascii_digit()).collect();
                    *errors.entry(general).or_default() += 1;
                }
                let count = tally.entry(label(&run)).or_default();
                count.seeds += 1;
                if count.shortest.as_ref().is_none_or(|s| case.cut_after < s.cut_after) {
                    count.shortest = Some(Shortest {
                        seed,
                        cut_after: case.cut_after,
                    });
                }
            }
            println!("seeds {from}..{}", from + count);
            for (kind, Count { seeds, shortest }) in tally {
                let Shortest { seed, cut_after } = shortest.expect("a kind is only counted with a seed");
                println!("  {kind:<34} {seeds:>6}   shortest: seed {seed} (cut after {cut_after})");
            }
            println!("errors on reopening:");
            for (error, n) in errors {
                println!("  {n:>6}  {error}");
            }
        }
        Some("every") => {
            let workload = Workload::generate(num(1));
            let crash_seeds = num(2);
            quiet_engine_panics();
            let total = workload.total_ops();
            let mut tally: BTreeMap<&str, u64> = BTreeMap::new();
            let mut broken_points = 0;
            for cut in 0..total {
                let mut broken = false;
                for crash_seed in 0..crash_seeds {
                    let run = run(&workload, cut, crash_seed);
                    broken |= !run.verdict.is_intact();
                    *tally.entry(label(&run)).or_default() += 1;
                }
                broken_points += u64::from(broken);
            }
            println!(
                "workload {}: {} commits, {total} cut points x {crash_seeds} crash seeds",
                num(1),
                workload.commits.len()
            );
            println!("cut points where at least one crash seed breaks the promise: {broken_points} of {total}");
            for (kind, n) in tally {
                println!("  {kind:<34} {n:>6}");
            }
        }
        _ => eprintln!("usage: crash_sim one <seed> | sweep <from> <count> | every <workload seed> <crash seeds>"),
    }
}
