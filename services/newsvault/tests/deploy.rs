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
            start.contains("feed-contact:/run/secrets/feed-contact:ro"),
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
