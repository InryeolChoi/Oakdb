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

/// For each kind of verdict: how many seeds, and the shortest example as
/// (seed, operations before the cut).
type Tally = BTreeMap<&'static str, (u64, Option<(u64, u64)>)>;

fn kind(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Intact { .. } => "intact",
        Verdict::OpenFailed(_) => "open failed",
        Verdict::Panicked(_) => "panicked",
        Verdict::ReadFailed(_) => "read failed",
        Verdict::LostAcknowledged(_) => "lost an acknowledged commit",
        Verdict::WrongValue(_) => "wrong value",
        Verdict::HalfCommit(_) => "half a commit",
    }
}

/// The verdict, and when the power went: while creating the database or
/// during a commit.
fn label(run: &Run) -> &'static str {
    let opening = run.cut_commit.is_none() && run.acked == 0;
    match (kind(&run.verdict), opening) {
        ("intact", _) => "intact",
        ("open failed", true) => "open failed (cut while creating)",
        ("open failed", false) => "open failed (cut in a commit)",
        ("read failed", _) => "read failed",
        ("panicked", _) => "panicked on reopening",
        ("lost an acknowledged commit", _) => "lost an acknowledged commit",
        ("wrong value", _) => "wrong value",
        ("half a commit", _) => "half a commit",
        _ => unreachable!(),
    }
}

fn describe(run: &Run) {
    println!("acknowledged commits: {}", run.acked);
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
        let what = match &w.landing {
            Landing::Lost => "lost".to_string(),
            Landing::Kept => "kept".to_string(),
            Landing::Torn { sectors } => format!(
                "torn, sectors {}",
                sectors
                    .iter()
                    .map(|&s| if s { '#' } else { '.' })
                    .collect::<String>()
            ),
        };
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
            let mut tally = Tally::new();
            let mut errors: BTreeMap<String, u64> = BTreeMap::new();
            // A panic on reopening is counted, not printed every time.
            std::panic::set_hook(Box::new(|_| {}));
            for seed in from..from + count {
                let case = Case::from_seed(seed);
                let run = case.run();
                if let Verdict::OpenFailed(e) | Verdict::ReadFailed(e) | Verdict::Panicked(e) = &run.verdict {
                    // "page 9 has ..." and "page 30 has ..." count together.
                    let general: String = e.chars().filter(|c| !c.is_ascii_digit()).collect();
                    *errors.entry(general).or_default() += 1;
                }
                let entry = tally.entry(label(&run)).or_default();
                entry.0 += 1;
                // The shortest example: the fewest operations before the cut.
                if entry.1.is_none_or(|(_, cut)| case.cut_after < cut) {
                    entry.1 = Some((seed, case.cut_after));
                }
            }
            println!("seeds {from}..{}", from + count);
            for (kind, (n, shortest)) in tally {
                let (seed, cut) = shortest.unwrap();
                println!("  {kind:<34} {n:>6}   shortest: seed {seed} (cut after {cut})");
            }
            println!("errors on reopening:");
            for (error, n) in errors {
                println!("  {n:>6}  {error}");
            }
        }
        Some("every") => {
            let workload = Workload::generate(num(1));
            let crash_seeds = num(2);
            std::panic::set_hook(Box::new(|_| {}));
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
            println!("cut points where some crash breaks the promise: {broken_points} of {total}");
            for (kind, n) in tally {
                println!("  {kind:<34} {n:>6}");
            }
        }
        _ => eprintln!("usage: crash_sim one <seed> | sweep <from> <count> | every <workload seed> <crash seeds>"),
    }
}
