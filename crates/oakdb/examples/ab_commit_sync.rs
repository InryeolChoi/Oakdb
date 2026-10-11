//! A/B experiment: how much of a commit is the disk sync?
//! (docs/experiments/0002-ab-commit-sync.md)
//!
//! `ab_commit_sync <rounds> <a-mode> <b-mode>` runs the same workload under two
//! sync modes and tests whether their commit latencies differ. A mode is one of:
//!
//! - `full`: `F_FULLFSYNC` on macOS, `fdatasync`/`fsync` on Linux (what `StdFs` does)
//! - `fsync`: plain `fsync` (on macOS this does not flush the drive's cache)
//! - `none`: no sync at all (not durable; the lower bound)
//!
//! Each round opens a fresh database and commits distinct small keys one per
//! transaction until the single page is full. Rounds alternate A and B so slow
//! drift in the machine hits both sides equally.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

use oakdb::fs::{Fs, FsFile};
use oakdb::{Db, Error};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Full,
    Fsync,
    None,
}

impl Mode {
    fn parse(s: &str) -> Mode {
        match s {
            "full" => Mode::Full,
            "fsync" => Mode::Fsync,
            "none" => Mode::None,
            _ => panic!("unknown mode {s}: use full, fsync or none"),
        }
    }
}

#[derive(Clone, Copy)]
struct ModeFs(Mode);

struct ModeFile(File, Mode);

fn sync(file: &File, mode: Mode) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is valid for as long as `file` is borrowed.
    let rc = match mode {
        Mode::None => 0,
        Mode::Fsync => unsafe { libc::fsync(fd) },
        #[cfg(target_os = "macos")]
        Mode::Full => unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) },
        #[cfg(target_os = "linux")]
        Mode::Full => unsafe { libc::fdatasync(fd) },
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Fs for ModeFs {
    type File = ModeFile;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)?;
        self.sync_dir(path)
    }

    fn open(&self, path: &Path) -> io::Result<ModeFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(ModeFile(file, self.0))
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        sync(&File::open(dir)?, self.0)
    }
}

impl FsFile for ModeFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.0.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.0.write_all_at(buf, offset)
    }
    fn size(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        sync(&self.0, self.1)
    }
}

/// Commit latencies of one round, in microseconds.
fn round(mode: Mode) -> Vec<f64> {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(ModeFs(mode), dir.path()).unwrap();
    let mut out = Vec::new();
    for i in 0u32.. {
        let mut txn = db.begin();
        txn.put(&i.to_be_bytes(), &i.to_be_bytes()).unwrap();
        let start = Instant::now();
        match txn.commit() {
            Ok(()) => out.push(start.elapsed().as_secs_f64() * 1e6),
            Err(Error::PageFull) => break,
            Err(e) => panic!("{e}"),
        }
    }
    out
}

fn median(xs: &[f64]) -> f64 {
    quantile(xs, 0.5)
}

fn quantile(xs: &[f64], q: f64) -> f64 {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// Two-sided Mann-Whitney U test with the normal approximation and tie
/// correction. Returns (U for A, z, p).
fn mann_whitney(a: &[f64], b: &[f64]) -> (f64, f64, f64) {
    let mut all: Vec<(f64, usize)> = a.iter().map(|&x| (x, 0)).collect();
    all.extend(b.iter().map(|&x| (x, 1)));
    all.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap());
    let n = all.len();
    let mut ranks = vec![0.0; n];
    let mut tie_term = 0.0;
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && all[j + 1].0 == all[i].0 {
            j += 1;
        }
        let r = (i + j) as f64 / 2.0 + 1.0;
        for rank in &mut ranks[i..=j] {
            *rank = r;
        }
        let t = (j - i + 1) as f64;
        tie_term += t * t * t - t;
        i = j + 1;
    }
    let (n1, n2) = (a.len() as f64, b.len() as f64);
    let r1: f64 = all
        .iter()
        .zip(&ranks)
        .filter(|((_, g), _)| *g == 0)
        .map(|(_, r)| r)
        .sum();
    let u1 = r1 - n1 * (n1 + 1.0) / 2.0;
    let mean = n1 * n2 / 2.0;
    let nn = n1 + n2;
    let var = n1 * n2 / 12.0 * ((nn + 1.0) - tie_term / (nn * (nn - 1.0)));
    let z = (u1 - mean) / var.sqrt();
    (u1, z, 2.0 * (1.0 - normal_cdf(z.abs())))
}

fn normal_cdf(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

/// Abramowitz-Stegun 7.1.26, absolute error below 1.5e-7.
fn erf(x: f64) -> f64 {
    let s = x.signum();
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    s * y
}

/// 95% bootstrap interval for median(A) / median(B).
fn bootstrap_ratio(a: &[f64], b: &[f64], iters: usize) -> (f64, f64) {
    let mut seed: u64 = 0x5eed_0a4b;
    let mut next = |len: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % len as u64) as usize
    };
    let mut ratios = Vec::with_capacity(iters);
    for _ in 0..iters {
        let ra: Vec<f64> = (0..a.len()).map(|_| a[next(a.len())]).collect();
        let rb: Vec<f64> = (0..b.len()).map(|_| b[next(b.len())]).collect();
        ratios.push(median(&ra) / median(&rb));
    }
    (quantile(&ratios, 0.025), quantile(&ratios, 0.975))
}

fn summary(name: &str, mode: Mode, xs: &[f64]) {
    println!(
        "{name} ({mode:?}): n={} p50={:.1}us p90={:.1}us p99={:.1}us",
        xs.len(),
        median(xs),
        quantile(xs, 0.9),
        quantile(xs, 0.99)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: ab_commit_sync <rounds> <a-mode> <b-mode>");
        std::process::exit(2);
    }
    let rounds: usize = args[1].parse().unwrap();
    let (ma, mb) = (Mode::parse(&args[2]), Mode::parse(&args[3]));

    // Warm up both paths once, then alternate.
    round(ma);
    round(mb);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for r in 0..rounds {
        if r % 2 == 0 {
            a.extend(round(ma));
            b.extend(round(mb));
        } else {
            b.extend(round(mb));
            a.extend(round(ma));
        }
    }

    println!("os={} rounds={rounds}", std::env::consts::OS);
    summary("A", ma, &a);
    summary("B", mb, &b);
    let (u, z, p) = mann_whitney(&a, &b);
    let (lo, hi) = bootstrap_ratio(&a, &b, 2000);
    println!("Mann-Whitney U={u:.0} z={z:.2} p={p:.3e}");
    println!(
        "median(A)/median(B) = {:.2}x, 95% bootstrap CI [{lo:.2}, {hi:.2}]",
        median(&a) / median(&b)
    );
}
