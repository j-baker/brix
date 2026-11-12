//! How many evaluation threads to use when nobody has said.
//!
//! Many processors mix fast cores with slower, more efficient ones. More
//! evaluation threads than the fast cores have hardware threads do not
//! pay: the evaluator stops every worker to collect garbage and workers
//! wait for each other's thunks, so a worker on a slow core holds the
//! fast ones back. (A 4 fast + 6 slow core machine checked one flake in
//! 18 s with 8 workers and in 23 to 26 s with 20.)

/// The number of hardware threads on this machine's performance cores
/// that this process may use: the default for `eval-cores`. On a
/// machine whose cores are all alike, or where the kernel does not say,
/// that is every thread available to the process.
pub fn performance_threads() -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    match max_frequencies() {
        Some(frequencies) => fastest(&frequencies).clamp(1, available),
        None => available,
    }
}

/// How many of the CPUs with these maximum frequencies are performance
/// cores' threads: those within 15% of the fastest. Cores of one kind
/// differ by a few percent where some are favoured for boosting;
/// efficiency cores reach three quarters of the performance cores'
/// frequency at most.
fn fastest(frequencies: &[u64]) -> usize {
    let highest = frequencies.iter().copied().max().unwrap_or(0);
    frequencies
        .iter()
        .filter(|&&frequency| frequency * 100 >= highest * 85)
        .count()
}

/// The maximum frequency of every online CPU, as Linux's cpufreq reports
/// it. `None` if any is missing (another system, a virtual machine).
fn max_frequencies() -> Option<Vec<u64>> {
    let mut frequencies = Vec::new();
    for entry in std::fs::read_dir("/sys/devices/system/cpu").ok()? {
        let path = entry.ok()?.path();
        let is_cpu = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("cpu"))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
        if !is_cpu {
            continue;
        }
        // The boot CPU may have no `online` file: it cannot go offline.
        if std::fs::read_to_string(path.join("online")).is_ok_and(|online| online.trim() == "0") {
            continue;
        }
        let frequency = std::fs::read_to_string(path.join("cpufreq/cpuinfo_max_freq")).ok()?;
        frequencies.push(frequency.trim().parse().ok()?);
    }
    (!frequencies.is_empty()).then_some(frequencies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_threads_are_the_fast_cores_threads() {
        // AMD Ryzen AI 9 365: 4 Zen 5 cores and 6 Zen 5c cores, two
        // threads each.
        let mut hybrid = vec![5_090_910; 8];
        hybrid.extend([3_324_676; 12]);
        assert_eq!(fastest(&hybrid), 8);
        // Intel Core i9-13900K: two favoured performance cores, six more,
        // and sixteen efficiency cores with one thread each.
        let mut favoured = vec![5_800_000; 4];
        favoured.extend([5_500_000; 12]);
        favoured.extend([4_300_000; 16]);
        assert_eq!(fastest(&favoured), 16);
        // Cores all alike.
        assert_eq!(fastest(&[4_200_000; 16]), 16);
        assert_eq!(fastest(&[]), 0);
    }

    #[test]
    fn this_machine_has_some() {
        let threads = performance_threads();
        assert!(threads >= 1);
        assert!(threads <= std::thread::available_parallelism().map_or(1, usize::from));
    }
}
