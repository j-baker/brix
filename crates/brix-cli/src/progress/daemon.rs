//! The daemon half of Nix's progress bar. IDs are kept separate from
//! fetcher IDs; counters come from resProgress, never inferred from a stop
//! (which can also mean a failed or cancelled build).

use std::collections::BTreeMap;
use std::time::Instant;

use brix_daemon::logger::{Activity, ActivityId, ActivityKind, ActivityType, Field, ResultType};

use super::{render_count, render_size, ANSI_NORMAL, TEXT_AFTER};

#[derive(Debug, Default)]
pub(super) struct State {
    pub activities: Vec<(ActivityId, Running)>,
    finished: BTreeMap<ActivityType, (u64, u64)>,
}

#[derive(Debug)]
pub(super) struct Running {
    kind: ActivityType,
    parent: ActivityId,
    text: String,
    phase: String,
    last_line: String,
    pub started: Instant,
    done: u64,
    expected: u64,
    running: u64,
    failed: u64,
    expected_by_type: BTreeMap<ActivityType, u64>,
    visible: bool,
}

fn string(fields: &[Field], i: usize) -> String {
    match fields.get(i) {
        Some(Field::Str(s)) => String::from_utf8_lossy(s).into_owned(),
        _ => String::new(),
    }
}

fn integer(fields: &[Field], i: usize) -> u64 {
    match fields.get(i) {
        Some(Field::Int(n)) => *n,
        _ => 0,
    }
}

fn path_name(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .split_once('-')
        .map_or(path, |(_, name)| name)
}

impl State {
    pub fn start(&mut self, activity: &Activity) {
        let (kind, fields) = match &activity.kind {
            ActivityKind::Typed { kind, fields } => (*kind, fields.as_slice()),
            ActivityKind::Named { .. } => (ActivityType::Unknown, &[][..]),
        };
        let path = string(fields, 0);
        let name = path_name(&path);
        let machine = string(fields, 1);
        let text = match kind {
            ActivityType::Build => format!(
                "building {}{}",
                name.trim_end_matches(".drv"),
                if machine.is_empty() {
                    String::new()
                } else {
                    format!(" on {machine}")
                }
            ),
            ActivityType::PostBuildHook => format!("post-build {}", name.trim_end_matches(".drv")),
            ActivityType::Substitute => format!(
                "{} {name} from {machine}",
                if machine.starts_with("local") {
                    "copying"
                } else {
                    "fetching"
                }
            ),
            ActivityType::QueryPathInfo => format!("querying {name} on {machine}"),
            _ => String::from_utf8_lossy(&activity.text).into_owned(),
        };
        let visible = !text.is_empty()
            && !(kind == ActivityType::FileTransfer
                && (self.has_ancestor(activity.parent, ActivityType::CopyPath)
                    || self.has_ancestor(activity.parent, ActivityType::QueryPathInfo))
                || kind == ActivityType::CopyPath
                    && self.has_ancestor(activity.parent, ActivityType::Substitute));
        self.activities.push((
            activity.id,
            Running {
                kind,
                parent: activity.parent,
                text,
                phase: String::new(),
                last_line: String::new(),
                started: Instant::now(),
                done: 0,
                expected: 0,
                running: 0,
                failed: 0,
                expected_by_type: BTreeMap::new(),
                visible,
            },
        ));
    }

    fn has_ancestor(&self, mut id: ActivityId, kind: ActivityType) -> bool {
        // Bound traversal even for a malformed cyclic activity graph.
        for _ in 0..self.activities.len() {
            let Some((_, r)) = self.activities.iter().find(|(i, _)| *i == id) else {
                break;
            };
            if r.kind == kind {
                return true;
            }
            id = r.parent;
        }
        false
    }

    pub fn stop(&mut self, id: ActivityId) {
        let Some(at) = self.activities.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let (_, r) = self.activities.remove(at);
        let totals = self.finished.entry(r.kind).or_default();
        totals.0 += r.done;
        totals.1 += r.failed;
        // expected_by_type disappears with its owner, as in Nix.
    }

    pub fn result(&mut self, id: ActivityId, kind: ResultType, fields: &[Field]) {
        let Some(at) = self.activities.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let r = &mut self.activities[at].1;
        match kind {
            ResultType::Progress => {
                r.done = integer(fields, 0);
                r.expected = integer(fields, 1);
                r.running = integer(fields, 2);
                r.failed = integer(fields, 3);
            }
            ResultType::SetExpected => {
                r.expected_by_type.insert(
                    ActivityType::from_wire(integer(fields, 0)),
                    integer(fields, 1),
                );
            }
            ResultType::SetPhase => r.phase = string(fields, 0),
            ResultType::BuildLogLine | ResultType::PostBuildLogLine | ResultType::FetchStatus => {
                r.last_line = string(fields, 0).trim_end().to_owned();
                // Nix brings the build that produced a line to the front.
                let activity = self.activities.remove(at);
                self.activities.push(activity);
            }
            _ => {}
        }
    }

    fn totals(&self, kind: ActivityType) -> (u64, u64, u64, u64) {
        let (mut done, mut failed) = self.finished.get(&kind).copied().unwrap_or_default();
        let mut expected = done;
        let mut running = 0;
        let mut announced = 0;
        for (_, r) in &self.activities {
            announced += r.expected_by_type.get(&kind).copied().unwrap_or_default();
            if r.kind == kind {
                done += r.done;
                expected += r.expected;
                running += r.running;
                failed += r.failed;
            }
        }
        (running, done, expected.max(announced), failed)
    }

    fn count(&self, kind: ActivityType, label: &str) -> String {
        let (running, done, expected, failed) = self.totals(kind);
        let count = render_count(running, done, expected);
        if count.is_empty() && failed == 0 {
            return String::new();
        }
        let mut s = format!("{} {label}", if count.is_empty() { "0" } else { &count });
        if failed > 0 {
            s.push_str(&format!(" (\x1b[31;1m{failed} failed{ANSI_NORMAL})"));
        }
        s
    }

    pub fn status(&self) -> Vec<String> {
        let mut parts = Vec::new();
        let builds = self.count(ActivityType::Builds, "built");
        if !builds.is_empty() {
            parts.push(builds);
        }
        let copied = self.count(ActivityType::CopyPaths, "copied");
        let (_, done, expected, _) = self.totals(ActivityType::CopyPath);
        let size = render_size(done, expected);
        if !copied.is_empty() || !size.is_empty() {
            let mut s = if copied.is_empty() {
                "0 copied".into()
            } else {
                copied
            };
            if !size.is_empty() {
                s.push_str(&format!(" ({size})"));
            }
            parts.push(s);
        }
        parts
    }

    pub fn download_totals(&self) -> (u64, u64) {
        let (_, done, expected, _) = self.totals(ActivityType::FileTransfer);
        (done, expected)
    }

    pub fn latest(&self, now: Instant) -> Option<String> {
        self.activities
            .iter()
            .rev()
            .find(|(_, r)| r.visible && now.saturating_duration_since(r.started) >= TEXT_AFTER)
            .map(|(_, r)| {
                let mut text = r.text.clone();
                if !r.phase.is_empty() {
                    text.push_str(&format!(" ({})", r.phase));
                }
                if !r.last_line.is_empty() {
                    text.push_str(&format!(": {}", r.last_line));
                }
                text
            })
    }
}
