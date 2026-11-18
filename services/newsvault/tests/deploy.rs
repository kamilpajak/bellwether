//! The files under `deploy/` against the code they schedule and the metrics they read
//! (architecture.md section 11.5 step 7, PR 6): a unit, a cron line or an alert rule that drifts
//! from `schedule::SLOTS` or from the watchdog's metric families is red here, not on the VPS.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

use chrono::TimeDelta;
use news_provider::Provider;
use newsvault::cmd_ledger::PROMETHEUS_FAMILIES;
use newsvault::schedule::{self, SLOTS};
use open_web_feed::OpenWebFeed;

type TestResult = Result<(), Box<dyn Error>>;

/// S7-D9: `TimeoutStartSec` is the longest window plus 2 min.
const TIMEOUT_MARGIN: TimeDelta = TimeDelta::seconds(120);
/// The watchdog timer's interval, an `OnCalendar` firing once an hour.
const WATCHDOG_INTERVAL_S: i64 = 3_600;
const CONTACT: &str = "bellwether collector contact@example.test";
/// A bind `--mount`, which refuses a missing source where `--volume` creates it as root's directory.
const CONTACT_MOUNT: &str =
    "--mount type=bind,src=%h/.config/bellwether/feed-contact,dst=/run/secrets/feed-contact,readonly";
const STORE_LOCK: &str =
    "/usr/bin/flock --nonblock --conflict-exit-code 7 %t/bellwether-store.lock";
/// The Dockerfile's base image and the runner of every workflow job: one Ubuntu release.
const UBUNTU_IMAGE: &str = "ARG UBUNTU=ubuntu:noble-";
const UBUNTU_RUNNER: &str = "ubuntu-24.04";
const WORKFLOWS: [&str; 3] = ["ci.yml", "image.yml", "deploy.yml"];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> Result<String, Box<dyn Error>> {
    Ok(std::fs::read_to_string(repo().join(relative))?)
}

/// Every value of `key` in a unit file, comment lines skipped and `\` continuations joined.
fn unit_values(text: &str, key: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut open = String::new();
    for line in text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
    {
        match line.strip_suffix('\\') {
            Some(head) => open.push_str(head),
            None => {
                open.push_str(line);
                lines.push(std::mem::take(&mut open));
            }
        }
    }
    let prefix = format!("{key}=");
    lines
        .iter()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.split_whitespace().collect::<Vec<&str>>().join(" "))
        .collect()
}

fn slots() -> Vec<(u32, u32)> {
    SLOTS.iter().map(|slot| (slot.hour, slot.minute)).collect()
}

fn patience() -> Result<TimeDelta, Box<dyn Error>> {
    let feed = OpenWebFeed::open(
        &repo().join("providers/open-web-feed/hosts/open_web_wires.hosts.json"),
        &repo().join("providers/open-web-feed/rules/open-web-feed.rules.json"),
        CONTACT,
    )?;
    Ok(TimeDelta::from_std(feed.transport().timeouts.global)?)
}

/// `TimeoutStartSec` derived, never fitted: section 11.2's longest window plus the margin.
fn derived_timeout() -> Result<i64, Box<dyn Error>> {
    let patience = patience()?;
    let schedule = schedule::committed(patience)?;
    Ok((schedule.longest_window(patience) + TIMEOUT_MARGIN).num_seconds())
}

fn integers(values: &[String]) -> Vec<i64> {
    values
        .iter()
        .filter_map(|value| value.parse().ok())
        .collect()
}

#[test]
fn the_collect_timer_fires_at_exactly_the_schedules_slots_in_utc() -> TestResult {
    let timer = read("deploy/systemd/newsvault-collect.timer")?;
    let fired: Vec<Option<(u32, u32)>> = unit_values(&timer, "OnCalendar")
        .iter()
        .map(|line| {
            let time = line.strip_prefix("*-*-* ")?.strip_suffix(":00 UTC")?;
            let (hour, minute) = time.split_once(':')?;
            Some((hour.parse().ok()?, minute.parse().ok()?))
        })
        .collect();
    assert_eq!(
        (
            fired,
            unit_values(&timer, "AccuracySec"),
            unit_values(&timer, "RandomizedDelaySec").len(),
            unit_values(&timer, "Persistent").len(),
        ),
        (
            slots().into_iter().map(Some).collect(),
            vec!["1s".to_owned()],
            0,
            0
        )
    );
    Ok(())
}

/// 2,670 s + 120 s = 2,790 s on the committed schedule; the refresh unit and the Kubernetes
/// jobs are held to the same bound.
#[test]
fn every_run_bound_is_the_longest_window_plus_two_minutes() -> TestResult {
    let timeout = derived_timeout()?;
    let collect = read("deploy/systemd/newsvault-collect.service")?;
    let refresh = read("deploy/systemd/newsvault-robots-refresh.service")?;
    let jobs = read("deploy/k8s/newsvault-collect.yaml")?;
    let deadlines: Vec<i64> = jobs
        .lines()
        .filter_map(|line| line.trim().strip_prefix("activeDeadlineSeconds: "))
        .filter_map(|value| value.parse().ok())
        .collect();
    assert_eq!(
        (
            timeout,
            integers(&unit_values(&collect, "TimeoutStartSec")),
            integers(&unit_values(&refresh, "TimeoutStartSec")),
            deadlines,
        ),
        (2_790, vec![timeout], vec![timeout], vec![timeout, timeout])
    );
    Ok(())
}

/// S7-D8 and S7-D9: exit 8 succeeds, nothing restarts, the run is expected once per slot, and the
/// contact reaches the container as a file, never as a value.
#[test]
fn the_collect_unit_succeeds_on_exit_8_never_restarts_and_mounts_the_contact() -> TestResult {
    let collect = read("deploy/systemd/newsvault-collect.service")?;
    let start = unit_values(&collect, "ExecStart").join(" ");
    let expected_runs = format!("--expected-runs {}", SLOTS.len());
    assert_eq!(
        (
            unit_values(&collect, "SuccessExitStatus"),
            unit_values(&collect, "Restart").len(),
            start.contains(&expected_runs),
            start.contains("--name newsvault-collect"),
            start.contains("--env BELLWETHER_FEED_CONTACT_FILE=/run/secrets/feed-contact"),
            start.contains("BELLWETHER_FEED_CONTACT="),
            start.contains(CONTACT_MOUNT),
        ),
        (vec!["8".to_owned()], 0, true, true, true, false, true)
    );
    Ok(())
}

/// One cron field of plain numbers, ranges and commas, as the two committed lines write it.
fn expand(field: &str) -> Option<Vec<u32>> {
    let mut values = Vec::new();
    for part in field.split(',') {
        match part.split_once('-') {
            Some((from, to)) => values.extend(from.parse::<u32>().ok()?..=to.parse().ok()?),
            None => values.push(part.parse().ok()?),
        }
    }
    Some(values)
}

/// Text from `jobTemplate:` to the end of its YAML document.
fn job_templates(jobs: &str) -> Vec<String> {
    jobs.split("\n---\n")
        .filter_map(|document| {
            let at = document.find("  jobTemplate:")?;
            document
                .get(at..)
                .map(|template| template.trim_end().to_owned())
        })
        .collect()
}

#[test]
fn the_kubernetes_cron_lines_cover_exactly_the_slots_and_share_one_job() -> TestResult {
    let jobs = read("deploy/k8s/newsvault-collect.yaml")?;
    let mut fired: Vec<(u32, u32)> = Vec::new();
    for line in jobs.lines() {
        let Some(cron) = line.trim().strip_prefix("schedule: ") else {
            continue;
        };
        let fields: Vec<&str> = cron.trim_matches('"').split(' ').collect();
        let [minute, hour, "*", "*", "*"] = fields.as_slice() else {
            return Err(format!("not a daily cron line: {line}").into());
        };
        let (minutes, hours) = (expand(minute).ok_or(line)?, expand(hour).ok_or(line)?);
        for hour in &hours {
            fired.extend(minutes.iter().map(|minute| (*hour, *minute)));
        }
    }
    fired.sort_unstable();
    let templates = job_templates(&jobs);
    let zones: Vec<&str> = jobs
        .lines()
        .filter_map(|line| line.trim().strip_prefix("timeZone: "))
        .collect();
    assert_eq!(
        (
            fired,
            zones,
            templates.len(),
            templates.windows(2).all(|pair| pair[0] == pair[1]),
            jobs.contains(&format!("--expected-runs {}", SLOTS.len())),
        ),
        (slots(), vec!["Etc/UTC"; 2], 2, true, true)
    );
    Ok(())
}

/// Every `newsvault_` name an alert rule's `expr` reads.
fn read_families(rules: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for word in rules.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if word.starts_with("newsvault_") {
            found.insert(word.to_owned());
        }
    }
    found
}

/// Each family the watchdog writes has a rule reading it, and each rule reads one it writes.
#[test]
fn every_metric_the_watchdog_writes_has_an_alert_rule_and_no_rule_reads_another() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let exprs: String = rules
        .split("- alert: ")
        .skip(1)
        .filter_map(|rule| rule.split("labels:").next())
        .collect();
    assert_eq!(
        read_families(&exprs),
        PROMETHEUS_FAMILIES
            .iter()
            .map(|family| (*family).to_owned())
            .collect::<BTreeSet<String>>()
    );
    Ok(())
}

/// The first `> N` threshold of one alert's `expr`.
fn threshold(rules: &str, alert: &str) -> Option<i64> {
    let rule = rules.split(&format!("- alert: {alert}\n")).nth(1)?;
    let expr = rule.split("labels:").next()?;
    let after = expr.split("> ").nth(1)?;
    after.split_whitespace().next()?.parse().ok()
}

/// Collect stale = two longest poll periods with no request (more than one slot missed); watchdog
/// stale = two of its hourly intervals with no fresh read.
#[test]
fn each_staleness_threshold_is_two_of_the_intervals_it_watches() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let timer = read("deploy/systemd/newsvault-watchdog.timer")?;
    let longest = schedule::committed(patience()?)?
        .longest_period()
        .num_seconds();
    let hourly = unit_values(&timer, "OnCalendar").iter().all(|line| {
        line.strip_prefix("*-*-* *:")
            .and_then(|rest| rest.strip_suffix(":00 UTC"))
            .is_some_and(|minute| minute.parse::<u32>().is_ok_and(|minute| minute < 60))
    });
    assert_eq!(
        (
            threshold(&rules, "NewsvaultCollectStale"),
            threshold(&rules, "NewsvaultWatchdogStale"),
            hourly,
            unit_values(&timer, "OnCalendar").len(),
        ),
        (Some(2 * longest), Some(2 * WATCHDOG_INTERVAL_S), true, 1)
    );
    Ok(())
}

/// The words of a unit's `ExecStart` before the image, the container's `--name` value replaced.
fn container_shape(unit: &str) -> (Vec<String>, Option<String>) {
    let start = unit_values(unit, "ExecStart").join(" ");
    let mut words = start
        .split(' ')
        .take_while(|word| *word != "${NEWSVAULT_IMAGE}");
    let mut shape = Vec::new();
    let mut name = None;
    while let Some(word) = words.next() {
        shape.push(word.to_owned());
        if word == "--name" {
            name = words.next().map(str::to_owned);
            shape.push("<name>".to_owned());
        }
    }
    (shape, name)
}

/// S7-D10's scheduler lock: a refresh and a collect take one lock and run one container shape, so
/// neither writes the store while the other does.
#[test]
fn the_collect_and_refresh_units_share_one_lock_and_one_container_shape() -> TestResult {
    let (collect, collect_name) =
        container_shape(&read("deploy/systemd/newsvault-collect.service")?);
    let (refresh, refresh_name) =
        container_shape(&read("deploy/systemd/newsvault-robots-refresh.service")?);
    let shape = collect.join(" ");
    assert_eq!(
        (
            collect == refresh,
            collect_name,
            refresh_name,
            shape.starts_with(STORE_LOCK),
            shape.contains(CONTACT_MOUNT),
        ),
        (
            true,
            Some("newsvault-collect".to_owned()),
            Some("newsvault-robots-refresh".to_owned()),
            true,
            true
        )
    );
    Ok(())
}

/// S7-D8's watchdog reads yesterday, which `NewsvaultRunsMissed` alerts on, with every slot due, so
/// a day no run reached is a shortfall of 31; `--check` gives the findings the rules read.
#[test]
fn the_watchdog_checks_yesterday_and_today_with_every_slot_expected() -> TestResult {
    let script = read("deploy/systemd/newsvault-watchdog")?.replace("\\\n", " ");
    let call = script
        .lines()
        .find(|line| line.contains(" ledger "))
        .map(|line| line.split_whitespace().collect::<Vec<&str>>().join(" "))
        .unwrap_or_default();
    let ask = format!(
        "ledger --at \"$at\" --from \"$yesterday\" --to \"$today\" --expected-runs {} --check \
         --format prometheus",
        SLOTS.len()
    );
    assert_eq!(
        (
            call.contains(&ask),
            call.contains("--network none"),
            call.contains("--mount \"type=bind,src=$HOME/.bellwether/vault,dst=/vault,readonly\""),
            script.contains("yesterday=$(date -u -d \"$today -1 day\" +%Y-%m-%d)"),
        ),
        (true, true, true, true),
        "{call}"
    );
    Ok(())
}

/// No host path reaches a container through `--volume`, which creates a missing one as root's.
#[test]
fn no_unit_or_script_mounts_a_host_path_with_volume() -> TestResult {
    let mut volumes = Vec::new();
    for file in [
        "deploy/systemd/newsvault-collect.service",
        "deploy/systemd/newsvault-robots-refresh.service",
        "deploy/systemd/newsvault-watchdog",
    ] {
        let text = read(file)?;
        volumes.extend(
            text.lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .filter(|line| line.contains("--volume") || line.contains(" -v "))
                .map(|line| format!("{file}: {line}")),
        );
    }
    assert_eq!(volumes, Vec::<String>::new());
    Ok(())
}

/// The gate links the system SQLite of the release the image is built on (3.45 on 24.04);
/// measured 2026-11-18, `ubuntu-latest` was 26.04, whose SQLite is 3.46.1.
#[test]
fn every_workflow_job_runs_on_the_images_ubuntu_release() -> TestResult {
    let mut runners = Vec::new();
    for workflow in WORKFLOWS {
        let text = read(&format!(".github/workflows/{workflow}"))?;
        let used: BTreeSet<String> = text
            .lines()
            .filter_map(|line| line.trim().strip_prefix("runs-on: "))
            .map(str::to_owned)
            .collect();
        runners.push((workflow, used.into_iter().collect::<Vec<String>>()));
    }
    assert_eq!(
        (runners, read("Dockerfile")?.contains(UBUNTU_IMAGE)),
        (
            WORKFLOWS
                .iter()
                .map(|workflow| (*workflow, vec![UBUNTU_RUNNER.to_owned()]))
                .collect::<Vec<_>>(),
            true
        )
    );
    Ok(())
}
