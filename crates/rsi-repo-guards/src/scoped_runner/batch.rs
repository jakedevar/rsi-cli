//! Exact libtest/nextest batches with bounded concurrency and real verdicts.
use super::{execution::Progress, process, selection};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::{atomic::AtomicUsize, mpsc};
use std::time::Instant;

type Names = BTreeSet<String>;
#[derive(Default, Debug)]
pub struct BatchResult {
    pub ran: Names,
    pub failures: Names,
    pub unfinished: Names,
    pub failed: bool,
    pub expired: bool,
    pub interrupted: Option<i32>,
}

pub struct BatchConfig<'a> {
    pub binary: &'a Path,
    pub directory: &'a Path,
    pub package: &'a str,
    pub kinds: &'a [String],
    pub target: &'a str,
    pub nextest: Option<&'a [String]>,
    pub features: &'a [String],
    pub threads: usize,
    pub env: &'a BTreeMap<String, String>,
    pub deadline: Instant,
    pub box_deadline: Option<Instant>,
    pub cancel: &'a AtomicUsize,
}

pub fn nextest_command(config: &BatchConfig<'_>, names: &[String]) -> Vec<String> {
    let mut command = config.nextest.unwrap_or_default().to_vec();
    command.extend(["run".into(), "-p".into(), config.package.into()]);
    if config.kinds.iter().any(|k| k == "lib") {
        command.push("--lib".into());
    } else {
        command.extend([
            if config.kinds.iter().any(|k| k == "bin") {
                "--bin"
            } else {
                "--test"
            }
            .into(),
            config.target.into(),
        ]);
    }
    if !config.features.is_empty() {
        command.extend(["--features".into(), config.features.join(",")]);
    }
    command.extend([
        "--no-fail-fast".into(),
        "--color".into(),
        "never".into(),
        "--status-level".into(),
        "pass".into(),
        "--final-status-level".into(),
        "none".into(),
        "--test-threads".into(),
        config.threads.to_string(),
        "--ignore-default-filter".into(),
        "-E".into(),
        names
            .iter()
            .map(|n| format!("test(={n})"))
            .collect::<Vec<_>>()
            .join(" | "),
    ]);
    command
}

#[derive(Default)]
struct NextestProgress {
    started: Names,
    done: Names,
    failures: Names,
}
impl NextestProgress {
    fn line(&mut self, line: &str, names: &Names, emit: &mut impl FnMut(&str)) {
        let pattern = crate::re::Re::new(
            r"^\s*([A-Z][A-Z-]+)\s+\[\s*>?\s*[\d.]+s\]\s+\([^)]*\)\s+(\S+)\s+(\S.*?)\s*$",
        );
        if let Some(caps) = pattern.search(line) {
            let status = caps.get(1).map_or("", |v| v.as_str());
            let name = caps.get(3).map_or("", |v| v.as_str());
            if names.contains(name) {
                let final_status = ![
                    "SLOW",
                    "TERMINATING",
                    "DELAY",
                    "RETRY",
                    "START",
                    "SKIP",
                    "CANCEL",
                    "CANCELLING",
                ]
                .contains(&status);
                if (status == "SLOW" || final_status) && self.started.insert(name.into()) {
                    emit(&format!("SCOPED_START {name}\n"));
                }
                if final_status && self.done.insert(name.into()) {
                    if !["PASS", "LEAK"].contains(&status) {
                        self.failures.insert(name.into());
                    }
                    emit(&format!("SCOPED_END {name}\n"));
                }
            }
        }
        emit(line);
    }
}

fn shard(
    config: &BatchConfig<'_>,
    names: Vec<String>,
    mut emit: impl FnMut(&str),
) -> io::Result<BatchResult> {
    let mut result = BatchResult::default();
    let safe = crate::re::Re::new(r"^[\w:]+$");
    // Unsafe filterset names keep libtest's exact argv semantics.
    let groups: Vec<(bool, Vec<String>)> = if config.nextest.is_some() {
        vec![
            (
                true,
                names.iter().filter(|n| safe.is_match(n)).cloned().collect(),
            ),
            (
                false,
                names
                    .iter()
                    .filter(|n| !safe.is_match(n))
                    .cloned()
                    .collect(),
            ),
        ]
    } else {
        vec![(false, names)]
    };
    for (nextest, names) in groups {
        let size = match (nextest, config.box_deadline.is_some()) {
            (true, true) => 120,
            (true, false) => 500,
            (false, true) => 32,
            (false, false) => 256,
        };
        for batch in selection::batches(&names, size, if nextest { 96 * 1024 } else { 64 * 1024 }) {
            // A box controls admission of batches, never turns an interrupted
            // test into a passing verdict. The package deadline still bounds it.
            if config.box_deadline.is_some_and(|d| Instant::now() >= d) {
                result.expired = true;
                return Ok(result);
            }
            let selected = Names::from_iter(batch.clone());
            let command = if nextest {
                nextest_command(config, &batch)
            } else {
                let mut command = vec![config.binary.to_string_lossy().into_owned()];
                command.extend(batch);
                command.extend([
                    "--exact".into(),
                    "--nocapture".into(),
                    "--test-threads=1".into(),
                ]);
                command
            };
            let outcome;
            let (done, failures, unfinished) = if nextest {
                let progress = RefCell::new(NextestProgress::default());
                outcome = process::stream(
                    &command,
                    config.directory,
                    config.env,
                    config.deadline,
                    config.cancel,
                    |line| progress.borrow_mut().line(line, &selected, &mut emit),
                    |_| {},
                )?;
                let progress = progress.into_inner();
                (progress.done, progress.failures, progress.started)
            } else {
                let progress = RefCell::new(Progress::default());
                let sink = RefCell::new(&mut emit);
                outcome = process::stream(
                    &command,
                    config.directory,
                    config.env,
                    config.deadline,
                    config.cancel,
                    |line| {
                        progress
                            .borrow_mut()
                            .line(line, &selected, &mut |line| sink.borrow_mut()(line))
                    },
                    |tail| {
                        progress
                            .borrow_mut()
                            .partial(tail, &selected, &mut |line| sink.borrow_mut()(line))
                    },
                )?;
                let progress = progress.into_inner();
                let unfinished = progress.active.into_iter().collect();
                if outcome.code == 0 && progress.executed == 0 {
                    result.failed = true;
                }
                (progress.done, progress.failures, unfinished)
            };
            result
                .unfinished
                .extend(unfinished.difference(&done).cloned());
            if outcome.code != 0 || !outcome.output_complete || done.len() != selected.len() {
                result.failed = true;
            }
            if outcome.code != 0 {
                emit(&format!("SCOPED_TEST_EXIT {}\n", outcome.code));
            }
            for failure in &failures {
                emit(&format!(
                    "SCOPED_FAILURE {}:{} {failure}\n",
                    config.kinds.first().map_or("lib", String::as_str),
                    config.target
                ));
            }
            result.ran.extend(done);
            result.failures.extend(failures);
            if outcome.timed_out || outcome.cancelled.is_some() {
                result.interrupted = Some(outcome.code);
                return Ok(result);
            }
        }
    }
    Ok(result)
}

pub fn run(
    config: &BatchConfig<'_>,
    names: &[String],
    mut emit: impl FnMut(&str),
) -> io::Result<BatchResult> {
    let shards = if config.nextest.is_some() {
        1
    } else {
        config.threads.min(names.len()).max(1)
    };
    let mut lists = vec![Vec::new(); shards];
    for (index, name) in names.iter().enumerate() {
        lists[index % shards].push(name.clone());
    }
    let (send, receive) = mpsc::sync_channel::<String>(64);
    std::thread::scope(|scope| {
        let handles: Vec<_> = lists
            .into_iter()
            .map(|names| {
                let send = send.clone();
                scope.spawn(move || {
                    shard(config, names, |line| {
                        let _ = send.send(line.into());
                    })
                })
            })
            .collect();
        drop(send);
        for line in receive {
            emit(&line);
        }
        let mut total = BatchResult::default();
        for handle in handles {
            let result = handle
                .join()
                .map_err(|_| io::Error::other("test shard panicked"))??;
            total.ran.extend(result.ran);
            total.failures.extend(result.failures);
            total.unfinished.extend(result.unfinished);
            total.failed |= result.failed;
            total.expired |= result.expired;
            if result.interrupted.is_some() {
                total.interrupted = result.interrupted;
            }
        }
        Ok(total)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nextest_only_counts_observed_final_verdicts() {
        let selected = Names::from(["suite::a".into(), "suite::b".into()]);
        let mut progress = NextestProgress::default();
        let mut output = String::new();
        for line in [
            " SLOW [ > 1.000s] (1/2) pkg suite::a\n",
            " PASS [ 1.100s] (1/2) pkg suite::a\n",
            " SKIP [ 0.000s] (2/2) pkg suite::b\n",
            " PASS [ 0.100s] (1/2) pkg suite::a\n",
        ] {
            progress.line(line, &selected, &mut |line| output.push_str(line));
        }
        assert_eq!(progress.done, Names::from(["suite::a".into()]));
        assert_eq!(output.matches("SCOPED_END suite::a").count(), 1);
    }
    #[test]
    fn libtest_partial_start_is_never_a_finished_test() {
        let selected = Names::from(["suite::a".into()]);
        let mut progress = Progress::default();
        let mut output = String::new();
        progress.partial("test suite::a ... ", &selected, &mut |line| {
            output.push_str(line)
        });
        assert_eq!(progress.active.as_deref(), Some("suite::a"));
        assert!(progress.done.is_empty());
        assert_eq!(output, "SCOPED_START suite::a\n");
    }
}
