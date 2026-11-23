//! The files under `deploy/` against the code they schedule and the metrics they read
//! (architecture.md section 11.5 step 7, PR 6): a unit, a cron line or an alert rule that drifts
//! from `schedule::SLOTS` or from the watchdog's metric families is red here, not on the VPS.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, TimeDelta, Utc};
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
/// Each workflow job that links or ships SQLite, and so checks it is 3.45.
const SQLITE_JOBS: [(&str, &str); 3] = [
    ("ci.yml", "check"),
    ("image.yml", "build"),
    ("image.yml", "publish"),
];
const SQLITE_CHECK: &str = r"--show libsqlite3-0 | grep -E '^3\.45\.'";
/// Each validator `deploy.yml` runs, by job, as the tail of its `run` line.
const VALIDATORS: [(&str, &str); 9] = [
    (
        "prometheus-rules",
        "--entrypoint /bin/promtool \"$PROMETHEUS\" check rules newsvault.rules.yml",
    ),
    (
        "prometheus-rules",
        "--entrypoint /bin/promtool \"$PROMETHEUS\" test rules newsvault.rules.test.yml",
    ),
    (
        "kubernetes-manifests",
        "./kubeconform -strict -summary -kubernetes-version 1.34.12 deploy/k8s/",
    ),
    (
        "systemd-units",
        "run: shellcheck deploy/systemd/newsvault-watchdog deploy/systemd/newsvault-backup \
         deploy/systemd/newsvault-unit-result deploy/systemd/modelgate-shadow \
         deploy/monitoring/newsvault-monitoring-check",
    ),
    (
        "systemd-units",
        "systemd-analyze --user --man=no verify deploy/systemd/*.service deploy/systemd/*.timer",
    ),
    (
        "monitoring-check",
        "run: MONITORING_DIR=\"$RUNNER_TEMP/monitoring\" sh deploy/monitoring/newsvault-monitoring-check",
    ),
    (
        "monitoring-check",
        "sh deploy/monitoring/newsvault-monitoring-check >\"$RUNNER_TEMP/drift.out\" || status=$?",
    ),
    ("monitoring-check", "test \"$status\" -eq 1"),
    (
        "monitoring-check",
        "grep -Ec '^drift (prometheus_reload|route NewsvaultHostHeld|repeat_interval):' \
         \"$RUNNER_TEMP/drift.out\" | grep -x 3",
    ),
];
const IMAGE_IDENTITY: &str =
    "docker inspect --format '{{.Config.User}} {{json .Config.Entrypoint}}'";
const IMAGE_IDENTITY_EXPECTED: &str =
    "| grep -Fx '10001 [\"/usr/bin/tini\",\"--\",\"/usr/local/bin/newsvault\"]'";
const SERVICES: [&str; 6] = [
    "modelgate-shadow.service",
    "newsvault-collect.service",
    "newsvault-robots-refresh.service",
    "newsvault-watchdog.service",
    "newsvault-backup.service",
    "newsvault-roster.service",
];
const ENV_FILE: &str = "%h/.config/bellwether/newsvault.env";
/// systemd's `DefaultTimeoutStopSec`: the stop a start past its timeout is given before SIGKILL.
const DEFAULT_TIMEOUT_STOP_S: i64 = 90;
const NTP_CHECK: &str = "/bin/sh -c 'test \"$$(timedatectl show --property=NTPSynchronized \
     --value)\" = yes || { echo \"the clock is not NTP-synchronised\" >&2; exit 1; }'";
const TINI_COMMAND: &str = "command: [\"/usr/bin/tini\", \"-g\", \"--\", \"/bin/sh\", \"-c\"]";
const WATCHDOG_DOCKER: &str = "/usr/bin/docker run";
const PROM: &str = "newsvault.prom";
const OLD_METRICS: &str = "newsvault_old 1\n";
/// Stands in for docker: lists the textfile directory while its stdout is open, records its
/// arguments one per line, and prints one sample naming its exit.
const DOCKER_STUB: &str = r#"ls -a "$NEWSVAULT_TEXTFILE_DIR" >"$STUB_SEEN"
printf '%s\n' "$@" >"$STUB_ARGS"
printf 'newsvault_stub{exit="%s"} 1\n' "$STUB_EXIT"
exit "$STUB_EXIT"
"#;
/// UTC+14, so a `date` without `-u` is 14 h from the UTC now.
const FAR_ZONE: &str = "Pacific/Kiritimati";
const AT_TOLERANCE_S: i64 = 600;
const UNIT_RESULT_FAMILIES: [&str; 3] = [
    "newsvault_unit_result_timestamp_seconds",
    "newsvault_unit_result_success",
    "newsvault_unit_exit_status",
];
const BACKUP_FAMILIES: [&str; 2] = [
    "newsvault_backup_last_success_timestamp_seconds",
    "newsvault_backup_last_success_bytes",
];
/// `deploy/systemd/modelgate-shadow`'s, from the run's envelope.
const SHADOW_FAMILIES: [&str; 4] = [
    "modelgate_shadow_spent_micro_usd",
    "modelgate_shadow_cap_micro_usd",
    "modelgate_shadow_cost_cap_reached",
    "modelgate_shadow_last_success_timestamp_seconds",
];
/// The shadow timer's interval: two firings an hour.
const SHADOW_INTERVAL_S: i64 = 1_800;
/// The image jobs' modelgate smoke: the wrapper's container flags, then a dry run's envelope.
const MODELGATE_SMOKE: (&str, &str) = (
    "run: docker run --rm --read-only --network none --user 1000:1000 --entrypoint /usr/bin/tini ",
    "--dry-run --format json | jq -e '.dry_run and .question_document == \"ripple-shadow/0\"'",
);
/// Written for a person reading the textfile or a dashboard; no alert reads them.
const UNREAD_FAMILIES: [&str; 1] = ["newsvault_backup_last_success_bytes"];
/// The roster fetch's requests: one robots.txt and one file (`cmd_roster::FILES`).
const ROSTER_REQUESTS: i32 = 2;
const DAY_S: i64 = 86_400;
const SCRIPTS: &str = "%h/.local/lib/bellwether";

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

/// Every `newsvault_` and `modelgate_` name an alert rule's `expr` reads.
fn read_families(rules: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for word in rules.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if word.starts_with("newsvault_") || word.starts_with("modelgate_") {
            found.insert(word.to_owned());
        }
    }
    found
}

/// Each family the watchdog and the scripts write has a rule reading it, but those written for a
/// person only, and each rule reads one of them; each script writes exactly its own families.
#[test]
fn every_metric_written_has_an_alert_rule_and_no_rule_reads_another() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let exprs: String = rules
        .split("- alert: ")
        .skip(1)
        .filter_map(|rule| rule.split("labels:").next())
        .collect();
    let owned = |families: &[&str]| -> BTreeSet<String> {
        families.iter().map(|family| (*family).to_owned()).collect()
    };
    let read_by_rules: BTreeSet<String> = PROMETHEUS_FAMILIES
        .iter()
        .chain(&UNIT_RESULT_FAMILIES)
        .chain(&BACKUP_FAMILIES)
        .chain(&SHADOW_FAMILIES)
        .filter(|family| !UNREAD_FAMILIES.contains(family))
        .map(|family| (*family).to_owned())
        .collect();
    assert_eq!(
        (
            read_families(&exprs),
            read_families(&read("deploy/systemd/newsvault-unit-result")?),
            read_families(&read("deploy/systemd/newsvault-backup")?),
            read_families(&read("deploy/systemd/modelgate-shadow")?),
        ),
        (
            read_by_rules,
            owned(&UNIT_RESULT_FAMILIES),
            owned(&BACKUP_FAMILIES),
            owned(&SHADOW_FAMILIES)
        )
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

/// S7-D10's scheduler lock: a refresh, a roster fetch and a collect take one lock and run one
/// container shape, so none writes the store while another does.
#[test]
fn the_collect_and_refresh_units_share_one_lock_and_one_container_shape() -> TestResult {
    let (collect, collect_name) =
        container_shape(&read("deploy/systemd/newsvault-collect.service")?);
    let (refresh, refresh_name) =
        container_shape(&read("deploy/systemd/newsvault-robots-refresh.service")?);
    let (roster, roster_name) = container_shape(&read("deploy/systemd/newsvault-roster.service")?);
    let shape = collect.join(" ");
    assert_eq!(
        (
            collect == refresh,
            collect == roster,
            collect_name,
            refresh_name,
            roster_name,
            shape.starts_with(STORE_LOCK),
            shape.contains(CONTACT_MOUNT),
        ),
        (
            true,
            true,
            Some("newsvault-collect".to_owned()),
            Some("newsvault-robots-refresh".to_owned()),
            Some("newsvault-roster".to_owned()),
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
        "deploy/systemd/newsvault-roster.service",
        "deploy/systemd/newsvault-watchdog",
        "deploy/systemd/newsvault-backup",
        "deploy/systemd/modelgate-shadow",
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
    let mut unchecked = Vec::new();
    for (workflow, name) in SQLITE_JOBS {
        let lines = job(&read(&format!(".github/workflows/{workflow}"))?, name);
        if !lines.iter().any(|line| line.ends_with(SQLITE_CHECK)) {
            unchecked.push(format!("{workflow} {name}"));
        }
    }
    assert_eq!(
        (
            runners,
            read("Dockerfile")?.contains(UBUNTU_IMAGE),
            unchecked
        ),
        (
            WORKFLOWS
                .iter()
                .map(|workflow| (*workflow, vec![UBUNTU_RUNNER.to_owned()]))
                .collect::<Vec<_>>(),
            true,
            Vec::<String>::new()
        )
    );
    Ok(())
}

/// Each line trimmed, comment lines dropped.
fn trimmed(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Whether `lines` holds `run` as consecutive lines.
fn holds(lines: &[String], run: &[&str]) -> bool {
    lines.windows(run.len()).any(|window| window == run)
}

/// One workflow job's lines, trimmed, from its `  name:` header to the next job.
fn job(workflow: &str, name: &str) -> Vec<String> {
    let header = format!("  {name}:");
    let body: Vec<&str> = workflow
        .lines()
        .skip_while(|line| *line != header)
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with("   "))
        .collect();
    trimmed(&body.join("\n"))
}

/// The lines under one top-level key of a workflow, trimmed.
fn top_level(workflow: &str, key: &str) -> Vec<String> {
    let header = format!("{key}:");
    workflow
        .lines()
        .skip_while(|line| *line != header)
        .skip(1)
        .take_while(|line| line.starts_with(' '))
        .map(|line| line.trim().to_owned())
        .collect()
}

/// `deploy.yml` runs each tool on the file it checks; a dropped step or flag is red here.
#[test]
fn the_deploy_workflow_runs_each_validator_on_its_file() -> TestResult {
    let deploy = read(".github/workflows/deploy.yml")?;
    let missing: Vec<&str> = VALIDATORS
        .iter()
        .filter(|(name, command)| {
            !job(&deploy, name)
                .iter()
                .any(|line| line.ends_with(command))
        })
        .map(|(_, command)| *command)
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
    Ok(())
}

/// Only a `v*` tag on a commit of main with a green `ci.yml` run publishes, and only that job
/// holds `packages: write`.
#[test]
fn only_a_tag_on_a_green_main_commit_publishes_and_only_that_job_writes_packages() -> TestResult {
    let image = read(".github/workflows/image.yml")?;
    let publish = job(&image, "publish");
    assert_eq!(
        (
            top_level(&image, "permissions"),
            publish.first().cloned(),
            image.matches("packages: write").count(),
            publish.contains(&"packages: write".to_owned()),
            holds(
                &publish,
                &[
                    "run: |",
                    "git merge-base --is-ancestor \"$GITHUB_SHA\" origin/main",
                    "green=$(gh api \"repos/$GITHUB_REPOSITORY/actions/workflows/ci.yml/runs?\
                     head_sha=$GITHUB_SHA&status=success\" --jq '.total_count')",
                    "test \"$green\" -gt 0",
                ]
            ),
            trimmed(&image).contains(&"tags: [\"v*\"]".to_owned()),
        ),
        (
            vec!["contents: read".to_owned()],
            Some("if: startsWith(github.ref, 'refs/tags/v')".to_owned()),
            1,
            true,
            true,
            true
        )
    );
    Ok(())
}

/// The Dockerfile's user and entrypoint, and each image job reading both back from the built
/// image; that reading runs only in CI.
#[test]
fn the_image_runs_as_user_10001_under_tini() -> TestResult {
    let dockerfile = trimmed(&read("Dockerfile")?);
    let image = read(".github/workflows/image.yml")?;
    let read_back: Vec<bool> = ["build", "publish"]
        .iter()
        .map(|name| {
            job(&image, name).iter().any(|line| {
                line.contains(IMAGE_IDENTITY) && line.ends_with(IMAGE_IDENTITY_EXPECTED)
            })
        })
        .collect();
    assert_eq!(
        (
            dockerfile.contains(&"USER 10001".to_owned()),
            dockerfile.contains(
                &"ENTRYPOINT [\"/usr/bin/tini\", \"--\", \"/usr/local/bin/newsvault\"]".to_owned()
            ),
            read_back,
        ),
        (true, true, vec![true, true])
    );
    Ok(())
}

/// A `simple` unit counts as started once its process forks, so `TimeoutStartSec` would bound
/// nothing; each unit reads the one env file.
#[test]
fn every_service_is_a_oneshot_reading_the_env_file() -> TestResult {
    let mut seen = Vec::new();
    for service in SERVICES {
        let unit = read(&format!("deploy/systemd/{service}"))?;
        seen.push((
            service,
            unit_values(&unit, "Type"),
            unit_values(&unit, "EnvironmentFile"),
        ));
    }
    assert_eq!(
        seen,
        SERVICES
            .map(|service| (
                service,
                vec!["oneshot".to_owned()],
                vec![ENV_FILE.to_owned()]
            ))
            .to_vec()
    );
    Ok(())
}

/// A hung read is stopped, its stop timeout included, before the next hourly firing, which a
/// still-active unit would swallow.
#[test]
fn a_hung_watchdog_is_stopped_before_its_next_firing() -> TestResult {
    let unit = read("deploy/systemd/newsvault-watchdog.service")?;
    let bounded: Vec<bool> = integers(&unit_values(&unit, "TimeoutStartSec"))
        .iter()
        .map(|start| start + DEFAULT_TIMEOUT_STOP_S < WATCHDOG_INTERVAL_S)
        .collect();
    assert_eq!(bounded, vec![true]);
    Ok(())
}

#[test]
fn each_timer_starts_its_own_service() -> TestResult {
    let mut started = Vec::new();
    for timer in [
        "newsvault-collect.timer",
        "newsvault-watchdog.timer",
        "newsvault-backup.timer",
        "newsvault-roster.timer",
        "modelgate-shadow.timer",
    ] {
        let unit = read(&format!("deploy/systemd/{timer}"))?;
        started.push((timer, unit_values(&unit, "Unit")));
    }
    assert_eq!(
        started,
        vec![
            (
                "newsvault-collect.timer",
                vec!["newsvault-collect.service".to_owned()]
            ),
            (
                "newsvault-watchdog.timer",
                vec!["newsvault-watchdog.service".to_owned()]
            ),
            (
                "newsvault-backup.timer",
                vec!["newsvault-backup.service".to_owned()]
            ),
            (
                "newsvault-roster.timer",
                vec!["newsvault-roster.service".to_owned()]
            ),
            (
                "modelgate-shadow.timer",
                vec!["modelgate-shadow.service".to_owned()]
            ),
        ]
    );
    Ok(())
}

/// The container's flags word for word; the refresh unit is held to the same words by
/// `the_collect_and_refresh_units_share_one_lock_and_one_container_shape`.
#[test]
fn the_collect_container_is_removed_read_only_unprivileged_and_never_pulled() -> TestResult {
    let (shape, _) = container_shape(&read("deploy/systemd/newsvault-collect.service")?);
    let expected = format!(
        "{STORE_LOCK} /usr/bin/docker run --rm --name <name> --pull never --user %U:%G \
         --read-only --tmpfs /tmp:rw,size=16m --cap-drop ALL --security-opt no-new-privileges \
         --mount type=bind,src=%h/.bellwether/vault,dst=/vault {CONTACT_MOUNT} \
         --env BELLWETHER_FEED_CONTACT_FILE=/run/secrets/feed-contact"
    );
    assert_eq!(shape.join(" "), expected);
    Ok(())
}

/// The `ExecStopPost` that removes a timed-out run's container by `name`.
fn removal(name: &str) -> String {
    format!(
        "/bin/sh -c 'if [ \"$$SERVICE_RESULT\" = timeout ]; then /usr/bin/docker rm --force \
         {name}; fi'"
    )
}

/// The `ExecStopPost` that writes a unit's result as metrics; `-`, so it never changes the result.
fn result_line(label: &str) -> String {
    format!("-{SCRIPTS}/newsvault-unit-result {label}")
}

/// A timed-out run's container is removed by the name it runs under, a collect's and a roster
/// fetch's result is written first, and each starts only on an NTP-synchronised clock.
#[test]
fn a_timed_out_container_is_removed_by_its_own_name_and_a_collect_needs_a_synced_clock(
) -> TestResult {
    let mut removals = Vec::new();
    let mut expected = Vec::new();
    for (service, result) in [
        ("newsvault-collect.service", Some("collect")),
        ("newsvault-robots-refresh.service", None),
        ("newsvault-roster.service", Some("roster")),
    ] {
        let unit = read(&format!("deploy/systemd/{service}"))?;
        let name = container_shape(&unit).1.unwrap_or_default();
        removals.push((service, unit_values(&unit, "ExecStopPost")));
        expected.push((
            service,
            result
                .map(result_line)
                .into_iter()
                .chain([removal(&name)])
                .collect::<Vec<String>>(),
        ));
    }
    let collect = read("deploy/systemd/newsvault-collect.service")?;
    let roster = read("deploy/systemd/newsvault-roster.service")?;
    assert_eq!(
        (
            removals,
            unit_values(&collect, "ExecStartPre"),
            unit_values(&roster, "ExecStartPre")
        ),
        (
            expected,
            vec![NTP_CHECK.to_owned()],
            vec![NTP_CHECK.to_owned()]
        )
    );
    Ok(())
}

/// The words after `${NEWSVAULT_IMAGE}` in a unit's `ExecStart`.
fn container_args(unit: &str) -> Vec<String> {
    unit_values(unit, "ExecStart")
        .join(" ")
        .split(' ')
        .skip_while(|word| *word != "${NEWSVAULT_IMAGE}")
        .skip(1)
        .map(str::to_owned)
        .collect()
}

/// Each Kubernetes job's `newsvault` arguments, the binary's path dropped.
fn job_args(jobs: &str) -> Vec<Vec<String>> {
    job_templates(jobs)
        .iter()
        .map(|template| {
            let mut words = Vec::new();
            let mut open = false;
            for line in template.lines().map(str::trim) {
                open = open || line.starts_with("/usr/local/bin/newsvault ");
                if open {
                    words.extend(
                        line.trim_end_matches('\\')
                            .split_whitespace()
                            .map(str::to_owned),
                    );
                    if !line.ends_with('\\') {
                        break;
                    }
                }
            }
            words.into_iter().skip(1).collect()
        })
        .collect()
}

/// The systemd collect and both Kubernetes jobs ask one `collect`, from one env file.
#[test]
fn the_collect_unit_asks_what_each_kubernetes_job_asks() -> TestResult {
    let args = container_args(&read("deploy/systemd/newsvault-collect.service")?);
    assert_eq!(
        (
            args.first().cloned(),
            job_args(&read("deploy/k8s/newsvault-collect.yaml")?)
        ),
        (Some("collect".to_owned()), vec![args.clone(); 2])
    );
    Ok(())
}

/// Each CronJob by value: no overlap, no retry, no restart, exit 8 mapped to 0 under `tini -g`,
/// exits 2, 4 and 7 named on the container that runs, a read-only root and secret, no capability.
#[test]
fn each_kubernetes_job_maps_exit_8_to_0_and_holds_its_limits() -> TestResult {
    let jobs = read("deploy/k8s/newsvault-collect.yaml")?;
    let mut missing = Vec::new();
    for document in jobs
        .split("\n---\n")
        .filter(|document| document.contains("kind: CronJob"))
    {
        let lines = trimmed(document);
        let container = lines
            .iter()
            .skip_while(|line| *line != "containers:")
            .nth(1)
            .and_then(|line| line.strip_prefix("- name: "))
            .unwrap_or_default();
        let named = format!("containerName: {container}");
        let runs: [&[&str]; 10] = [
            &["concurrencyPolicy: Forbid"],
            &["startingDeadlineSeconds: 60"],
            &["backoffLimit: 0"],
            &["restartPolicy: Never"],
            &[TINI_COMMAND],
            &[
                "status=$?",
                "if [ \"$status\" -eq 8 ]; then exit 0; fi",
                "exit \"$status\"",
            ],
            &[
                "podFailurePolicy:",
                "rules:",
                "- action: FailJob",
                "onExitCodes:",
                &named,
                "operator: In",
                "values: [2, 4, 7]",
            ],
            &["readOnlyRootFilesystem: true"],
            &["capabilities:", "drop: [ALL]"],
            &[
                "- name: feed-contact",
                "mountPath: /run/secrets/bellwether",
                "readOnly: true",
            ],
        ];
        missing.push(
            runs.iter()
                .filter(|run| !holds(&lines, run))
                .map(|run| run.join(" / "))
                .collect::<Vec<String>>(),
        );
    }
    assert_eq!(missing, vec![Vec::<String>::new(); 2]);
    Ok(())
}

/// One run of the watchdog script, its docker replaced by `DOCKER_STUB` exiting `status`.
struct Watched {
    exit: Option<i32>,
    files: Vec<String>,
    prom: String,
    tmp: Vec<String>,
    at: Option<String>,
}

fn watch(status: i32) -> Result<Watched, Box<dyn Error>> {
    let work = tempfile::TempDir::new()?;
    let textfile = work.path().join("textfile");
    std::fs::create_dir(&textfile)?;
    std::fs::write(textfile.join(PROM), OLD_METRICS)?;
    let stub = work.path().join("docker");
    let (seen, args, script) = (
        work.path().join("seen"),
        work.path().join("args"),
        work.path().join("watchdog"),
    );
    std::fs::write(&stub, DOCKER_STUB)?;
    let text = read("deploy/systemd/newsvault-watchdog")?;
    if text.matches(WATCHDOG_DOCKER).count() != 1 {
        return Err(format!("the watchdog runs `{WATCHDOG_DOCKER}` once").into());
    }
    let stubbed = format!("/bin/sh '{}' run", stub.display());
    std::fs::write(&script, text.replace(WATCHDOG_DOCKER, &stubbed))?;
    let output = Command::new("/bin/sh")
        .arg(&script)
        .env("NEWSVAULT_IMAGE", "newsvault:test")
        .env("NEWSVAULT_TEXTFILE_DIR", &textfile)
        .env("STUB_EXIT", status.to_string())
        .env("STUB_SEEN", &seen)
        .env("STUB_ARGS", &args)
        .env("TZ", FAR_ZONE)
        .stdin(Stdio::null())
        .output()?;
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&textfile)? {
        files.push(entry?.file_name().to_string_lossy().into_owned());
    }
    files.sort();
    let tmp = std::fs::read_to_string(&seen)?
        .lines()
        .filter(|name| ![".", "..", PROM].contains(name))
        .map(str::to_owned)
        .collect();
    let at = std::fs::read_to_string(&args)?
        .lines()
        .skip_while(|word| *word != "--at")
        .nth(1)
        .map(str::to_owned);
    Ok(Watched {
        exit: output.status.code(),
        files,
        prom: std::fs::read_to_string(textfile.join(PROM))?,
        tmp,
        at,
    })
}

/// The ledger's 0, 1 and 8 replace `newsvault.prom` with what it printed through one hidden
/// non-`.prom` file beside it, which the collector never reads; any other exit leaves the last
/// file and no temporary one.
#[test]
fn the_watchdog_renames_a_hidden_file_for_0_1_and_8_and_keeps_the_last_file_otherwise() -> TestResult
{
    let mut got = Vec::new();
    let mut expected = Vec::new();
    for status in [0, 1, 8, 2] {
        let watched = watch(status)?;
        let hidden: Vec<bool> = watched
            .tmp
            .iter()
            .map(|name| name.starts_with('.') && !name.ends_with(".prom"))
            .collect();
        got.push((status, watched.exit, watched.files, watched.prom, hidden));
        let (exit, prom) = if status == 2 {
            (Some(2), OLD_METRICS.to_owned())
        } else {
            (Some(0), format!("newsvault_stub{{exit=\"{status}\"}} 1\n"))
        };
        expected.push((status, exit, vec![PROM.to_owned()], prom, vec![true]));
    }
    assert_eq!(got, expected);
    Ok(())
}

/// `--at` is the UTC now on a host 14 h ahead of UTC.
#[test]
fn the_watchdog_reads_at_the_utc_now_whatever_the_hosts_zone() -> TestResult {
    let watched = watch(0)?;
    let off = match watched.at.as_deref().map(DateTime::parse_from_rfc3339) {
        Some(Ok(at)) => Some((Utc::now() - at.with_timezone(&Utc)).num_seconds().abs()),
        _ => None,
    };
    assert!(
        off.is_some_and(|off| off < AT_TOLERANCE_S),
        "{:?}",
        watched.at
    );
    Ok(())
}

/// One daily `OnCalendar=*-*-* HH:MM:00 UTC` per line, as seconds after 00:00 UTC.
fn daily_times(timer: &str) -> Vec<Option<i64>> {
    unit_values(timer, "OnCalendar")
        .iter()
        .map(|line| {
            let time = line.strip_prefix("*-*-* ")?.strip_suffix(":00 UTC")?;
            let (hour, minute) = time.split_once(':')?;
            Some(hour.parse::<i64>().ok()? * 3_600 + minute.parse::<i64>().ok()? * 60)
        })
        .collect()
}

/// Each slot's `(free from, next slot)`: a collect is done at its start plus `TimeoutStartSec`
/// plus systemd's stop timeout; the last slot's next is the first one a day later.
fn gaps(collect_bound: i64) -> Vec<(i64, i64)> {
    let starts: Vec<i64> = SLOTS
        .iter()
        .map(|slot| i64::from(slot.hour) * 3_600 + i64::from(slot.minute) * 60)
        .collect();
    let first = starts.first().copied().unwrap_or_default();
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let next = starts.get(index + 1).copied().unwrap_or(first + DAY_S);
            (start + collect_bound, next)
        })
        .collect()
}

/// The backup and a collect never meet: the backup starts after one slot's hard bound and its own
/// bound ends before the next slot, in a gap as long as the longest (2,520 s: 90 min less 2,790 s
/// less 90 s), once a day, never run late.
#[test]
fn the_backup_runs_once_a_day_inside_the_longest_gap_the_collect_runs_leave() -> TestResult {
    let timer = read("deploy/systemd/newsvault-backup.timer")?;
    let service = read("deploy/systemd/newsvault-backup.service")?;
    let gaps = gaps(derived_timeout()? + DEFAULT_TIMEOUT_STOP_S);
    let longest = gaps.iter().map(|(from, next)| next - from).max();
    let bound: Vec<i64> = integers(&unit_values(&service, "TimeoutStartSec"));
    let mut fitting = Vec::new();
    for (at, bound) in daily_times(&timer).iter().flatten().zip(&bound) {
        for (from, next) in &gaps {
            let fits =
                |start: i64| *from <= start && start + bound + DEFAULT_TIMEOUT_STOP_S <= *next;
            if fits(*at) || fits(at + DAY_S) {
                fitting.push(next - from);
            }
        }
    }
    assert_eq!(
        (
            fitting,
            longest,
            unit_values(&timer, "AccuracySec"),
            unit_values(&timer, "RandomizedDelaySec").len(),
            unit_values(&timer, "Persistent").len(),
            daily_times(&timer).len(),
        ),
        (
            longest.into_iter().collect::<Vec<i64>>(),
            Some(2_520),
            vec!["1s".to_owned()],
            0,
            0,
            1
        )
    );
    Ok(())
}

/// Two daily intervals plus one run's bound: the stamp is the run's start and appears only after
/// its copy is written and scanned, so at two intervals alone one missed day fired while the next
/// run wrote its copy.
#[test]
fn the_backup_staleness_threshold_is_two_daily_intervals_plus_one_runs_bound() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let timer = read("deploy/systemd/newsvault-backup.timer")?;
    let service = read("deploy/systemd/newsvault-backup.service")?;
    let bound = integers(&unit_values(&service, "TimeoutStartSec"));
    assert_eq!(
        (
            threshold(&rules, "NewsvaultBackupStale"),
            daily_times(&timer).iter().flatten().count(),
            bound.len(),
        ),
        (
            bound
                .first()
                .map(|bound| 2 * DAY_S + bound + DEFAULT_TIMEOUT_STOP_S),
            1,
            1
        )
    );
    Ok(())
}

/// The collect's own store lock, `--nonblock`: a backup that meets a run exits 7 and never holds
/// a slot back; its result is written first, and a timed-out snapshot's container is removed.
#[test]
fn the_backup_unit_takes_the_store_lock_without_waiting_and_records_its_result() -> TestResult {
    let unit = read("deploy/systemd/newsvault-backup.service")?;
    assert_eq!(
        (
            unit_values(&unit, "ExecStart"),
            unit_values(&unit, "ExecStopPost"),
            unit_values(&unit, "Restart").len(),
            unit_values(&unit, "SuccessExitStatus").len(),
        ),
        (
            vec![format!("{STORE_LOCK} {SCRIPTS}/newsvault-backup")],
            vec![
                result_line("backup"),
                format!("-{}", removal("newsvault-backup"))
            ],
            0,
            0
        )
    );
    Ok(())
}

/// `ci.yml`'s check that the compiler it runs is the pinned one.
const RUSTC_IS_THE_PIN: &str = r#"rustc --version | grep -F "rustc $(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml) ""#;

/// `rust-toolchain.toml` is the one home of the toolchain: the Dockerfile and `ci.yml` install it
/// from that file and name no version, override or toolchain variable of their own; the only
/// `rustup` command either runs is the bare `rustup toolchain install`.
#[test]
fn one_rust_toolchain_is_named_only_in_rust_toolchain_toml() -> TestResult {
    let pin = read("rust-toolchain.toml")?;
    let channel = pin
        .lines()
        .find_map(|line| line.strip_prefix("channel = \"")?.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned();
    let numeric = channel
        .split('.')
        .filter(|part| part.parse::<u32>().is_ok())
        .count();
    let dockerfile = trimmed(&read("Dockerfile")?);
    let ci = read(".github/workflows/ci.yml")?;
    let check = job(&ci, "check");
    let mut named_elsewhere = Vec::new();
    for (file, text) in [
        ("Dockerfile", dockerfile.join("\n")),
        ("ci.yml", trimmed(&ci).join("\n")),
    ] {
        for word in [
            channel.as_str(),
            "RUST_TOOLCHAIN",
            "RUSTUP_TOOLCHAIN",
            "toolchain:",
            "cargo +",
        ] {
            if text.contains(word) {
                named_elsewhere.push(format!("{file}: {word}"));
            }
        }
        for line in text.lines().filter(|line| line.contains("rustup ")) {
            if !(line.ends_with("rustup toolchain install") && line.matches("rustup ").count() == 1)
            {
                named_elsewhere.push(format!("{file}: {line}"));
            }
        }
    }
    let installs = |lines: &[String]| {
        lines
            .iter()
            .any(|line| line.ends_with("rustup toolchain install"))
    };
    assert_eq!(
        (
            numeric,
            pin.contains("components = [\"clippy\", \"rustfmt\"]"),
            named_elsewhere,
            dockerfile
                .iter()
                .any(|line| line.contains("--default-toolchain none")),
            dockerfile.contains(&"COPY rust-toolchain.toml .".to_owned()),
            installs(&dockerfile),
            holds(&check, &["rustup toolchain install", RUSTC_IS_THE_PIN]),
        ),
        (3, true, Vec::<String>::new(), true, true, true, true)
    );
    Ok(())
}

const OLD_BACKUP_METRICS: &str = "newsvault_backup_last_success_bytes 1\n";
const BACKUP_PROM: &str = "newsvault-backup.prom";
const SNAPSHOT_BYTES: &str = "the snapshot";
const NC_SETTINGS: [&str; 4] = [
    "NEXTCLOUD_CONTAINER",
    "NEXTCLOUD_DATA",
    "NEXTCLOUD_USER",
    "NEXTCLOUD_FOLDER",
];
/// The template's values as the container sees them, with the test's Nextcloud user.
const NC_DATA: &str = "/mnt/ncdata";
const NC_USER: &str = "ncuser";
const NC_DIR: &str = "/mnt/ncdata/ncuser/files/Backups/bellwether";
const NC_SCAN: &str = "php occ files:scan --path=ncuser/files/Backups/bellwether";
const NC_KNOWN: &str = "php occ info:file /ncuser/files/Backups/bellwether";
const EXEC: &str = "exec -u www-data nextcloud-aio-nextcloud";
const EXEC_STDIN: &str = "exec -i -u www-data nextcloud-aio-nextcloud";
const WRITE: &str = "sh -c cat >\"$1\" && wc -c <\"$1\" sh";
/// Copies already in the folder, oldest first.
const COPIES: [&str; 8] = [
    "bellwether-vault-20000101T225500Z.tar.gz",
    "bellwether-vault-20000102T225500Z.tar.gz",
    "bellwether-vault-20000103T225500Z.tar.gz",
    "bellwether-vault-20000104T225500Z.tar.gz",
    "bellwether-vault-20000105T225500Z.tar.gz",
    "bellwether-vault-20000106T225500Z.tar.gz",
    "bellwether-vault-20000107T225500Z.tar.gz",
    "bellwether-vault-20000108T225500Z.tar.gz",
];
/// Files of another shape, which no run deletes; the last three are near misses of a staging name.
const OTHERS: [&str; 5] = [
    "bellwether-vault-20000101T225500Z.tar.gz.bak",
    "notes.txt",
    ".notes.part",
    "x.bellwether-vault-20000101T225500Z.tar.gz.part",
    ".bellwether-vault-20000101T225500Z.tar.gz.part.keep",
];
/// A staging file a killed run left, which the next run's retention removes.
const STALE_PARTIAL: &str = ".bellwether-vault-20000101T225500Z.tar.gz.part";
/// How long `terminate_during_write` waits for the write to start: 200 × 50 ms.
const TERM_POLLS: u32 = 200;
const TERM_POLL_MS: u64 = 50;
/// The shell the unit runs the script with: Ubuntu's `/bin/sh`.
const UNIT_SHELL: &str = "/bin/dash";
/// A copy whose name sorts after any copy a run writes today.
const FUTURE_COPY: &str = "bellwether-vault-20990101T225500Z.tar.gz";
/// Stands in for docker. `run`: records its argv, then writes a snapshot into the directory
/// mounted on `/out`. `exec`: records its argv on one line, fails the step `STUB_FAIL` names,
/// and runs the command on the host with `/mnt/ncdata` mapped to `$STUB_DIR/ncdata`.
const BACKUP_DOCKER_STUB: &str = r#"#!/bin/sh
if [ "$1" = run ]; then
    printf '%s\n' "$@" >>"$STUB_DIR/docker-args"
    for arg; do
        case $arg in type=bind,src=*,dst=/out) out=${arg#type=bind,src=}; out=${out%,dst=/out} ;; esac
    done
    [ "${STUB_DOCKER_EXIT:-0}" = 0 ] || exit "$STUB_DOCKER_EXIT"
    printf 'the snapshot' >"$out/commit.sqlite3"
    echo '{"schema": "newsvault.snapshot/1"}'
    exit 0
fi
echo "$*" >>"$STUB_DIR/execs"
shift
while :; do
    case $1 in -i) shift ;; -u) shift 2 ;; *) break ;; esac
done
shift
case $1 in
    sh) step=write ;;
    mv) step=rename ;;
    ls) step=list ;;
    php) if [ "$3" = info:file ]; then step=known
         elif [ -e "$STUB_DIR/scanned" ]; then step=rescan; else step=scan; : >"$STUB_DIR/scanned"; fi ;;
    *) step=$1 ;;
esac
[ "$step" = "${STUB_FAIL:-}" ] && exit 1
n=$#
for arg; do
    case $arg in /mnt/ncdata/*) arg="$STUB_DIR/ncdata/${arg#/mnt/ncdata/}" ;; esac
    set -- "$@" "$arg"
done
shift "$n"
case $step in
    write)
        if [ "${STUB_SHORT_WRITE:-}" = yes ]; then head -c 5 >"$5"; wc -c <"$5"; exit; fi
        if [ "${STUB_SLOW_WRITE:-}" = yes ]; then exec sleep 30; fi
        shift; exec /bin/sh "$@" ;;
    list) [ "${STUB_LISTING_LACKS_NEW:-}" = yes ] && exec cat "$STUB_DIR/listed"; shift; exec /bin/ls "$@" ;;
    mkdir | rm | rename) cmd=$1; shift; exec "/bin/$cmd" "$@" ;;
    scan | rescan) exit 0 ;;
    known) [ -f "$STUB_DIR/ncdata$4" ] && exit 0; echo "file $4 not found"; exit 1 ;;
esac
exit 127
"#;
/// Records its argv, then moves.
const MV_STUB: &str =
    "#!/bin/sh\nprintf '%s\\n' \"$@\" >>\"$STUB_DIR/mv-args\"\nexec /bin/mv \"$@\"\n";

fn executable(path: &Path, text: &str) -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// How one backup run is set up to go wrong, if at all.
#[derive(Default, Clone, Copy)]
struct Trouble {
    /// The container step that exits 1: mkdir, write, rename, scan, known (`occ info:file`), list,
    /// rm or rescan.
    fail: Option<&'static str>,
    short_write: bool,
    /// The write sleeps and the run's process group gets SIGTERM once the write has started.
    terminated: bool,
    listing_lacks_new: bool,
    /// The folder also holds `FUTURE_COPY`.
    future_copy: bool,
    docker_exit: Option<i32>,
    /// A Nextcloud setting replacing the template's, `None` unsetting it.
    setting: Option<(&'static str, Option<&'static str>)>,
    /// `Some(n)`: the folder holds the n oldest `COPIES` only; `None`: all, `OTHERS` and
    /// `STALE_PARTIAL`.
    copies: Option<usize>,
    folder_absent: bool,
}

/// One run of `deploy/systemd/newsvault-backup` against a stub docker and mv.
struct Backed {
    exit: Option<i32>,
    output: String,
    /// The new copy's name, read from the write's staging path.
    name: String,
    execs: Vec<String>,
    docker_args: Vec<String>,
    mv_args: Vec<String>,
    /// The folder's names after the run.
    folder: Vec<String>,
    archive: Vec<u8>,
    metric: String,
    old_link: String,
    textfile: Vec<String>,
    staging: Vec<String>,
}

fn sorted_names(dir: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn lines_of(path: &Path) -> Vec<String> {
    read_or_empty(path).lines().map(str::to_owned).collect()
}

/// The template's `KEY=value` lines.
fn env_template() -> Result<Vec<(String, String)>, Box<dyn Error>> {
    Ok(read("deploy/systemd/newsvault.env")?
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect())
}

/// The names a folder is seeded with for `trouble`.
fn seeded(trouble: Trouble) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = match trouble.copies {
        Some(count) => COPIES.iter().take(count).copied().collect(),
        None => COPIES
            .iter()
            .chain(&OTHERS)
            .chain(&[STALE_PARTIAL])
            .copied()
            .collect(),
    };
    if trouble.future_copy {
        names.push(FUTURE_COPY);
    }
    names
}

/// Runs `command` in its own process group and, once `execs` records the write, sends the group
/// SIGTERM, as systemd's stop does to the unit's cgroup.
fn terminate_during_write(
    command: &mut Command,
    execs: &Path,
) -> Result<std::process::Output, Box<dyn Error>> {
    use std::os::unix::process::CommandExt;
    let child = command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    for _ in 0..TERM_POLLS {
        if lines_of(execs)
            .iter()
            .any(|call| call.starts_with(EXEC_STDIN))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(TERM_POLL_MS));
    }
    // dash's `kill -TERM -- -<pid>` exits 2 "Illegal number" and signals nothing (CI, 2026-11-18).
    let killed = Command::new(UNIT_SHELL)
        .args([
            "-c",
            "kill -s TERM -- \"-$1\"",
            "sh",
            &child.id().to_string(),
        ])
        .status()?;
    let output = child.wait_with_output()?;
    if !killed.success() {
        return Err(format!("kill of the run's process group exited {killed}").into());
    }
    Ok(output)
}

fn backup(trouble: Trouble) -> Result<Backed, Box<dyn Error>> {
    let work = tempfile::TempDir::new()?;
    let (home, stub, textfile) = (
        work.path().join("home"),
        work.path().join("stub"),
        work.path().join("textfile"),
    );
    let vault = home.join(".bellwether/vault");
    let folder = stub.join("ncdata/ncuser/files/Backups/bellwether");
    for dir in [
        vault.join("blobs/sha256/ab"),
        vault.join("runs"),
        stub.join("bin"),
        stub.join("ncdata/ncuser/files"),
        textfile.clone(),
    ] {
        std::fs::create_dir_all(dir)?;
    }
    for (file, text) in [
        ("commit.sqlite3", "the live log"),
        ("commit.sqlite3-journal", "a journal"),
        ("LOCK", ""),
        ("blobs/sha256/ab/abcd", "a blob"),
        ("runs/20261118T225500Z.json", "a run file"),
        (".kept", "a hidden file"),
    ] {
        std::fs::write(vault.join(file), text)?;
    }
    if !trouble.folder_absent {
        std::fs::create_dir_all(&folder)?;
        for name in seeded(trouble) {
            std::fs::write(folder.join(name), "an old copy")?;
        }
    }
    std::fs::write(stub.join("listed"), seeded(trouble).join("\n") + "\n")?;
    std::fs::write(textfile.join(BACKUP_PROM), OLD_BACKUP_METRICS)?;
    std::fs::hard_link(textfile.join(BACKUP_PROM), work.path().join("old-link"))?;
    executable(&stub.join("bin/docker"), BACKUP_DOCKER_STUB)?;
    executable(&stub.join("bin/mv"), MV_STUB)?;
    // `bx` matches the glob in an unsafe test value, so a check that expands globs admits it.
    let cwd = work.path().join("cwd");
    std::fs::create_dir_all(cwd.join("bx"))?;
    let flag = |set: bool| if set { "yes" } else { "no" };
    let mut command = Command::new(UNIT_SHELL);
    command
        .arg(repo().join("deploy/systemd/newsvault-backup"))
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", stub.join("bin").display()),
        )
        .env("NEWSVAULT_IMAGE", "newsvault:test")
        .env("NEWSVAULT_TEXTFILE_DIR", &textfile)
        .env("STUB_DIR", &stub)
        .env("STUB_FAIL", trouble.fail.unwrap_or("none"))
        .env("STUB_SHORT_WRITE", flag(trouble.short_write))
        .env("STUB_SLOW_WRITE", flag(trouble.terminated))
        .env("STUB_LISTING_LACKS_NEW", flag(trouble.listing_lacks_new))
        .env(
            "STUB_DOCKER_EXIT",
            trouble.docker_exit.unwrap_or(0).to_string(),
        )
        .stdin(Stdio::null());
    for (key, value) in env_template()? {
        if NC_SETTINGS.contains(&key.as_str()) {
            command.env(key, value);
        }
    }
    command.env("NEXTCLOUD_USER", NC_USER);
    match trouble.setting {
        Some((key, Some(value))) => command.env(key, value),
        Some((key, None)) => command.env_remove(key),
        None => &mut command,
    };
    let output = if trouble.terminated {
        terminate_during_write(&mut command, &stub.join("execs"))?
    } else {
        command.output()?
    };
    let execs = lines_of(&stub.join("execs"));
    let name = execs
        .iter()
        .find(|call| call.starts_with(EXEC_STDIN))
        .and_then(|call| call.rsplit('/').next())
        .and_then(|staged| staged.strip_prefix('.'))
        .and_then(|staged| staged.strip_suffix(".part"))
        .unwrap_or_default()
        .to_owned();
    Ok(Backed {
        exit: output.status.code(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        archive: std::fs::read(folder.join(&name)).unwrap_or_default(),
        name,
        execs,
        docker_args: lines_of(&stub.join("docker-args")),
        mv_args: lines_of(&stub.join("mv-args")),
        folder: sorted_names(&folder).unwrap_or_default(),
        metric: read_or_empty(&textfile.join(BACKUP_PROM)),
        old_link: read_or_empty(&work.path().join("old-link")),
        textfile: sorted_names(&textfile)?,
        staging: sorted_names(&home.join(".cache/bellwether")).unwrap_or_default(),
    })
}

/// The archive's file members and the bytes of its `commit.sqlite3`, read with `tar`.
fn members(archive: &[u8]) -> Result<(Vec<String>, String), Box<dyn Error>> {
    let work = tempfile::TempDir::new()?;
    let (file, into) = (work.path().join("a.tar.gz"), work.path().join("into"));
    std::fs::create_dir(&into)?;
    std::fs::write(&file, archive)?;
    let listed = Command::new("tar").arg("-tzf").arg(&file).output()?;
    let unpacked = Command::new("tar")
        .arg("-xzf")
        .arg(&file)
        .arg("-C")
        .arg(&into)
        .status()?;
    let mut names: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .map(|name| name.trim_start_matches("./").to_owned())
        .filter(|name| !name.ends_with('/'))
        .collect();
    names.sort();
    let log = if unpacked.success() {
        read_or_empty(&into.join("commit.sqlite3"))
    } else {
        String::new()
    };
    Ok((names, log))
}

fn sorted(names: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    names.sort();
    names
}

/// One archive holding the snapshot as `commit.sqlite3` and every other store file but the live
/// log, its journal and `LOCK`, written to a hidden name in the folder, renamed, scanned; then the
/// oldest copies beyond 7 and a killed run's staging file deleted and the folder scanned again.
#[test]
fn the_backup_writes_through_the_container_renames_scans_then_keeps_the_seven_newest() -> TestResult
{
    let backed = backup(Trouble::default())?;
    let (names, log) = members(&backed.archive)?;
    let (dir, name) = (NC_DIR, &backed.name);
    let mut kept = vec![name.as_str()];
    kept.extend(&COPIES[2..]);
    kept.extend(&OTHERS);
    assert_eq!(
        (backed.exit, backed.execs, names, log, backed.folder),
        (
            Some(0),
            vec![
                format!("{EXEC} mkdir -p -- {dir}"),
                format!("{EXEC_STDIN} {WRITE} {dir}/.{name}.part"),
                format!("{EXEC} mv -- {dir}/.{name}.part {dir}/{name}"),
                format!("{EXEC} {NC_SCAN}"),
                format!("{EXEC} {NC_KNOWN}/{name}"),
                format!("{EXEC} ls -1A -- {dir}"),
                format!(
                    "{EXEC} rm -f -- {dir}/{} {dir}/{} {dir}/{STALE_PARTIAL}",
                    COPIES[0], COPIES[1]
                ),
                format!("{EXEC} {NC_SCAN}"),
            ],
            vec![
                ".kept".to_owned(),
                "blobs/sha256/ab/abcd".to_owned(),
                "commit.sqlite3".to_owned(),
                "runs/20261118T225500Z.json".to_owned(),
            ],
            SNAPSHOT_BYTES.to_owned(),
            sorted(&kept),
        ),
        "{}",
        backed.output
    );
    Ok(())
}

/// From an absent folder to 8 copies before the run, the folder ends with the 7 newest, the new
/// copy among them; a run that deletes nothing scans once.
#[test]
fn the_folder_keeps_exactly_the_seven_newest_copies_whatever_it_held() -> TestResult {
    let mut got = Vec::new();
    for (copies, absent) in [(0, true), (5, false), (6, false), (7, false), (8, false)] {
        let backed = backup(Trouble {
            copies: Some(copies),
            folder_absent: absent,
            ..Trouble::default()
        })?;
        let scans = backed
            .execs
            .iter()
            .filter(|call| call.ends_with(NC_SCAN))
            .count();
        got.push((copies, backed.exit, backed.folder.len(), scans));
    }
    let newest = backup(Trouble {
        copies: Some(8),
        ..Trouble::default()
    })?;
    let mut kept = vec![newest.name.as_str()];
    kept.extend(&COPIES[2..]);
    assert_eq!(
        (got, newest.folder),
        (
            vec![
                (0, Some(0), 1, 1),
                (5, Some(0), 6, 1),
                (6, Some(0), 7, 1),
                (7, Some(0), 7, 2),
                (8, Some(0), 7, 2),
            ],
            sorted(&kept)
        )
    );
    Ok(())
}

/// Every container call runs as `www-data`, only the write attaches stdin, and nothing runs
/// privileged, as root or through `sudo`.
#[test]
fn every_container_call_runs_as_www_data_and_only_the_write_reads_stdin() -> TestResult {
    let backed = backup(Trouble::default())?;
    let script = read("deploy/systemd/newsvault-backup")?;
    let as_www: Vec<bool> = backed
        .execs
        .iter()
        .map(|call| call.starts_with(&format!("{EXEC} ")))
        .collect();
    let with_stdin = backed
        .execs
        .iter()
        .filter(|call| call.starts_with(&format!("{EXEC_STDIN} {WRITE} ")))
        .count();
    let forbidden = ["--privileged", "-u root", "--user root", "sudo", "-u 0"]
        .iter()
        .filter(|word| {
            script.contains(*word) || backed.execs.iter().any(|call| call.contains(*word))
        })
        .count();
    assert_eq!(
        (backed.exit, as_www, with_stdin, forbidden),
        (
            Some(0),
            vec![true, false, true, true, true, true, true, true],
            1,
            0
        )
    );
    Ok(())
}

/// A write whose byte count differs from the archive's size is never renamed: exit 7, its
/// staging file removed, no copy deleted, the last metric kept.
#[test]
fn a_short_write_is_never_renamed_and_its_staging_file_is_removed() -> TestResult {
    let backed = backup(Trouble {
        short_write: true,
        ..Trouble::default()
    })?;
    let partial = format!(".{}.part", backed.name);
    let renames = backed
        .execs
        .iter()
        .filter(|call| call.contains(" mv "))
        .count();
    assert_eq!(
        (
            backed.exit,
            renames,
            backed.folder.contains(&partial),
            backed.folder.contains(&backed.name),
            backed.folder.len(),
            backed.metric,
            backed.output.contains("not renamed"),
        ),
        (
            Some(7),
            0,
            false,
            false,
            seeded(Trouble::default()).len(),
            OLD_BACKUP_METRICS.to_owned(),
            true
        ),
        "{}",
        backed.output
    );
    Ok(())
}

/// A failed mkdir, write, rename or scan, or a scan after which `occ info:file` does not find the
/// copy, exits 7 naming its step, deletes no copy, leaves no staging file and keeps the last metric.
#[test]
fn a_failed_mkdir_write_rename_or_scan_exits_7_by_name_and_deletes_nothing() -> TestResult {
    let mut got = Vec::new();
    for (step, said) in [
        ("mkdir", "creating"),
        ("write", "writing"),
        ("rename", "renaming"),
        ("scan", "files:scan"),
        ("known", "does not know"),
    ] {
        let backed = backup(Trouble {
            fail: Some(step),
            ..Trouble::default()
        })?;
        let partial = format!(".{}.part", backed.name);
        let copies_left = COPIES
            .iter()
            .filter(|copy| backed.folder.contains(&(**copy).to_owned()))
            .count();
        got.push((
            step,
            backed.exit,
            copies_left,
            backed.folder.contains(&partial),
            backed.metric == OLD_BACKUP_METRICS,
            backed.output.contains(said),
        ));
    }
    assert_eq!(
        got,
        vec![
            ("mkdir", Some(7), 8, false, true, true),
            ("write", Some(7), 8, false, true, true),
            ("rename", Some(7), 8, false, true, true),
            ("scan", Some(7), 8, false, true, true),
            ("known", Some(7), 8, false, true, true),
        ]
    );
    Ok(())
}

/// The copy is in and scanned, so its metric is written; then a failed listing, a listing without
/// the copy, a failed deletion or a failed second scan exits 8 naming its step. The first two
/// delete nothing.
#[test]
fn a_failed_retention_exits_8_after_the_metric_and_a_bad_listing_deletes_nothing() -> TestResult {
    let mut got = Vec::new();
    for (trouble, said) in [
        (
            Trouble {
                fail: Some("list"),
                ..Trouble::default()
            },
            "failed; nothing deleted",
        ),
        (
            Trouble {
                listing_lacks_new: true,
                ..Trouble::default()
            },
            "does not end with",
        ),
        (
            Trouble {
                fail: Some("rm"),
                ..Trouble::default()
            },
            "deleting",
        ),
        (
            Trouble {
                fail: Some("rescan"),
                ..Trouble::default()
            },
            "after the deletion",
        ),
    ] {
        let backed = backup(trouble)?;
        let copies_left = COPIES
            .iter()
            .filter(|copy| backed.folder.contains(&(**copy).to_owned()))
            .count();
        got.push((
            backed.exit,
            copies_left,
            backed.folder.contains(&backed.name),
            backed.metric == OLD_BACKUP_METRICS,
            backed.output.contains(said),
        ));
    }
    assert_eq!(
        got,
        vec![
            (Some(8), 8, true, false, true),
            (Some(8), 8, true, false, true),
            (Some(8), 8, true, false, true),
            (Some(8), 6, true, false, true),
        ]
    );
    Ok(())
}

/// A copy whose name sorts after the new one (a clock that ran ahead, a file put there by hand)
/// would make the new copy the oldest: the run exits 8 naming it and deletes nothing.
#[test]
fn a_copy_sorting_after_the_new_one_stops_the_retention_and_deletes_nothing() -> TestResult {
    let backed = backup(Trouble {
        future_copy: true,
        ..Trouble::default()
    })?;
    let mut held: Vec<&str> = seeded(Trouble {
        future_copy: true,
        ..Trouble::default()
    });
    held.push(&backed.name);
    assert_eq!(
        (
            backed.exit,
            backed.folder,
            backed.metric == OLD_BACKUP_METRICS,
            backed
                .output
                .contains(&format!("its newest copy is '{FUTURE_COPY}'")),
        ),
        (Some(8), sorted(&held), false, true),
        "{}",
        backed.output
    );
    Ok(())
}

/// SIGTERM during the write, as systemd's `TimeoutStartSec` sends it, still runs the `EXIT` trap
/// under dash: exit 130, the work directory gone, the staging file deleted in the container.
#[test]
fn a_terminated_run_exits_130_and_removes_its_work_directory_and_staging_file() -> TestResult {
    let backed = backup(Trouble {
        terminated: true,
        ..Trouble::default()
    })?;
    let removed = format!("{EXEC} rm -f -- {NC_DIR}/.{}.part", backed.name);
    assert_eq!(
        (
            backed.exit,
            backed.staging,
            backed.name.is_empty(),
            backed.execs.last() == Some(&removed),
        ),
        (Some(130), Vec::<String>::new(), false, true),
        "{:?}",
        backed.execs
    );
    Ok(())
}

/// Each Nextcloud setting is refused by name before any docker call when unset, empty, relative
/// where a path must be absolute (or the reverse), holding `..`, an empty part, a trailing `/`, or
/// a character outside letters, digits, `.`, `_` and `-`; a safe value of another shape runs.
#[test]
fn an_unsafe_nextcloud_setting_is_refused_by_name_before_any_docker_call() -> TestResult {
    let unsafe_values: [(&str, Option<&str>); 19] = [
        ("NEXTCLOUD_CONTAINER", None),
        ("NEXTCLOUD_CONTAINER", Some("")),
        ("NEXTCLOUD_CONTAINER", Some("--privileged")),
        ("NEXTCLOUD_CONTAINER", Some("nextcloud/aio")),
        ("NEXTCLOUD_DATA", Some("mnt/ncdata")),
        ("NEXTCLOUD_DATA", Some("/mnt/../etc")),
        ("NEXTCLOUD_DATA", Some("/")),
        ("NEXTCLOUD_DATA", Some("/mnt/ncdata/")),
        ("NEXTCLOUD_USER", Some("..")),
        ("NEXTCLOUD_USER", Some("ncuser/files")),
        ("NEXTCLOUD_USER", Some("<nextcloud-user-id>")),
        ("NEXTCLOUD_FOLDER", Some("/Backups")),
        ("NEXTCLOUD_FOLDER", Some("Backups/../../etc")),
        ("NEXTCLOUD_FOLDER", Some("Backups//bellwether")),
        ("NEXTCLOUD_FOLDER", Some("Back ups")),
        ("NEXTCLOUD_FOLDER", Some("Backups/$(id)")),
        ("NEXTCLOUD_FOLDER", Some("Backups/b*")),
        ("NEXTCLOUD_FOLDER", Some("Backups/a\nb")),
        ("NEXTCLOUD_FOLDER", Some("Backups/.hidden")),
    ];
    let mut refused = Vec::new();
    for (key, value) in unsafe_values {
        let backed = backup(Trouble {
            setting: Some((key, value)),
            ..Trouble::default()
        })?;
        refused.push((
            key,
            value,
            backed.exit,
            backed.docker_args.len() + backed.execs.len(),
            backed.output.contains(key),
        ));
    }
    let safe = backup(Trouble {
        setting: Some(("NEXTCLOUD_FOLDER", Some("Backups/bell_wether-2.x"))),
        ..Trouble::default()
    })?;
    let expected: Vec<_> = unsafe_values
        .iter()
        .map(|(key, value)| (*key, *value, Some(2), 0, true))
        .collect();
    assert_eq!(
        (
            refused,
            safe.exit,
            safe.execs
                .first()
                .cloned()
                .unwrap_or_default()
                .ends_with(&format!("{NC_DATA}/ncuser/files/Backups/bell_wether-2.x"))
        ),
        (expected, Some(0), true)
    );
    Ok(())
}

/// The metric is the archive's size and the snapshot's time, written after the scan to a hidden
/// non-`.prom` file beside `newsvault-backup.prom` and renamed onto it: the old inode keeps the
/// old text.
#[test]
fn the_backup_writes_its_metric_through_a_hidden_file_renamed_after_the_scan() -> TestResult {
    let backed = backup(Trouble::default())?;
    let at: Option<i64> = backed
        .metric
        .lines()
        .find_map(|line| line.strip_prefix("newsvault_backup_last_success_timestamp_seconds "))
        .and_then(|value| value.parse().ok());
    let hidden = moved_beside(&backed.mv_args, BACKUP_PROM);
    assert_eq!(
        (
            backed.metric.contains(&format!(
                "newsvault_backup_last_success_bytes {}\n",
                backed.archive.len()
            )),
            backed.archive.is_empty(),
            at.is_some_and(|at| (Utc::now().timestamp() - at).abs() < AT_TOLERANCE_S),
            backed.old_link,
            hidden,
            backed.textfile,
        ),
        (
            true,
            false,
            true,
            OLD_BACKUP_METRICS.to_owned(),
            true,
            vec![BACKUP_PROM.to_owned()]
        ),
        "{}",
        backed.metric
    );
    Ok(())
}

/// Whether `mv`'s one move put a dot-file beside `target`, named for it plus a suffix the
/// collector skips, onto `target`.
fn moved_beside(args: &[String], target: &str) -> bool {
    let paths: Vec<&Path> = args
        .iter()
        .filter(|word| !word.starts_with('-'))
        .map(Path::new)
        .collect();
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let [from, to] = paths.as_slice() else {
        return false;
    };
    from.parent() == to.parent()
        && name(to).as_deref() == Some(target)
        && name(from).is_some_and(|from| {
            from.starts_with(&format!(".{target}.")) && !from.ends_with(".prom")
        })
}

/// A refused snapshot ends the run with the snapshot's exit and no container call.
#[test]
fn a_refused_snapshot_stops_before_any_container_call() -> TestResult {
    let refused = backup(Trouble {
        docker_exit: Some(7),
        ..Trouble::default()
    })?;
    assert_eq!(
        (refused.exit, refused.execs, refused.metric),
        (Some(7), Vec::<String>::new(), OLD_BACKUP_METRICS.to_owned())
    );
    Ok(())
}

/// The work directory under `~/.cache/bellwether` holds a whole snapshot and archive; every run,
/// a failed one included, removes it.
#[test]
fn every_run_removes_its_staging_copy_whether_it_succeeds_or_fails() -> TestResult {
    let mut left = Vec::new();
    for trouble in [
        Trouble::default(),
        Trouble {
            fail: Some("write"),
            ..Trouble::default()
        },
        Trouble {
            short_write: true,
            ..Trouble::default()
        },
        Trouble {
            fail: Some("scan"),
            ..Trouble::default()
        },
        Trouble {
            listing_lacks_new: true,
            ..Trouble::default()
        },
        Trouble {
            docker_exit: Some(7),
            ..Trouble::default()
        },
    ] {
        let backed = backup(trouble)?;
        left.push((backed.exit, backed.staging));
    }
    assert_eq!(
        left,
        vec![
            (Some(0), Vec::new()),
            (Some(7), Vec::new()),
            (Some(7), Vec::new()),
            (Some(7), Vec::new()),
            (Some(8), Vec::new()),
            (Some(7), Vec::new()),
        ]
    );
    Ok(())
}

/// The snapshot container: removed, never pulled, read-only, no network, no capability, the store
/// mounted read-only and only the work directory writable.
#[test]
fn the_backup_container_reads_the_store_read_only_with_no_network() -> TestResult {
    let backed = backup(Trouble::default())?;
    let args = backed.docker_args.join(" ");
    let (flags, command) = args.split_at(args.find(" newsvault:test ").unwrap_or_default());
    let mounts: Vec<&str> = flags
        .split(' ')
        .filter_map(|word| word.strip_prefix("type=bind,src="))
        .map(|mount| mount.split_once(",dst=").map_or(mount, |(_, dst)| dst))
        .collect();
    let flags = flags
        .split(' ')
        .filter(|word| !word.starts_with("type=bind,src=") && !word.contains(':'))
        .collect::<Vec<&str>>()
        .join(" ");
    assert_eq!(
        (flags, mounts, command.trim()),
        (
            "run --rm --name newsvault-backup --pull never --user --read-only --network none \
             --cap-drop ALL --security-opt no-new-privileges --mount --mount"
                .to_owned(),
            vec!["/vault,readonly", "/out"],
            "newsvault:test snapshot --out /out/commit.sqlite3 --non-interactive --format json"
        )
    );
    Ok(())
}

/// One `newsvault-unit-result collect` run with systemd's three variables, as `(value lines, the
/// old inode's text, whether mv moved a hidden file beside the target)`.
fn unit_result(
    service: &str,
    code: &str,
    status: &str,
) -> Result<(String, String, bool), Box<dyn Error>> {
    let work = tempfile::TempDir::new()?;
    let (stub, textfile) = (work.path().join("stub"), work.path().join("textfile"));
    std::fs::create_dir_all(stub.join("bin"))?;
    std::fs::create_dir(&textfile)?;
    let target = textfile.join("newsvault-collect-result.prom");
    std::fs::write(&target, OLD_METRICS)?;
    std::fs::hard_link(&target, work.path().join("old-link"))?;
    executable(&stub.join("bin/mv"), MV_STUB)?;
    let output = Command::new("/bin/sh")
        .arg(repo().join("deploy/systemd/newsvault-unit-result"))
        .arg("collect")
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", stub.join("bin").display()),
        )
        .env("NEWSVAULT_TEXTFILE_DIR", &textfile)
        .env("SERVICE_RESULT", service)
        .env("EXIT_CODE", code)
        .env("EXIT_STATUS", status)
        .env("STUB_DIR", &stub)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    let hidden = moved_beside(
        &lines_of(&stub.join("mv-args")),
        "newsvault-collect-result.prom",
    );
    let values = read_or_empty(&target)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.contains("_timestamp_seconds"))
        .collect::<Vec<&str>>()
        .join(" ");
    Ok((values, read_or_empty(&work.path().join("old-link")), hidden))
}

/// systemd's `SERVICE_RESULT` as success, its `EXIT_STATUS` as the status for an exited process and
/// -1 for a signalled one, through a hidden file renamed onto the target.
#[test]
fn the_unit_result_names_success_and_the_exit_status_through_a_renamed_hidden_file() -> TestResult {
    let mut got = Vec::new();
    for (service, code, status) in [
        ("success", "exited", "0"),
        ("success", "exited", "8"),
        ("exit-code", "exited", "2"),
        ("timeout", "killed", "TERM"),
    ] {
        got.push(unit_result(service, code, status)?);
    }
    let expected = |success: u8, status: &str| {
        (
            format!(
                "newsvault_unit_result_success{{unit=\"collect\"}} {success} \
                 newsvault_unit_exit_status{{unit=\"collect\"}} {status}"
            ),
            OLD_METRICS.to_owned(),
            true,
        )
    };
    assert_eq!(
        got,
        vec![
            expected(1, "0"),
            expected(1, "8"),
            expected(0, "2"),
            expected(0, "-1"),
        ]
    );
    Ok(())
}

/// The scripts that write node-exporter textfiles, each with the arguments its unit passes.
const TEXTFILE_SCRIPTS: [(&str, &[&str]); 4] = [
    ("deploy/systemd/modelgate-shadow", &[]),
    ("deploy/systemd/newsvault-watchdog", &[]),
    ("deploy/systemd/newsvault-unit-result", &["collect"]),
    ("deploy/systemd/newsvault-backup", &[]),
];

/// The template's textfile directory is a placeholder, since installs differ: each script refuses
/// it by name with exit 2 before docker runs or a file is written. Mutations 2026-11-19, each
/// script's check removed and the template a path again: each red.
#[test]
fn the_templates_textfile_placeholder_is_refused_by_every_script_before_it_runs() -> TestResult {
    let template = env_template()?;
    let placeholder = template
        .iter()
        .find(|(key, _)| key == "NEWSVAULT_TEXTFILE_DIR")
        .map(|(_, value)| value.clone())
        .ok_or("the template sets no NEWSVAULT_TEXTFILE_DIR")?;
    let mut seen = Vec::new();
    for (script, args) in TEXTFILE_SCRIPTS {
        let work = tempfile::TempDir::new()?;
        let (stub, cwd, home) = (
            work.path().join("bin"),
            work.path().join("cwd"),
            work.path().join("home"),
        );
        for dir in [&stub, &cwd, &home.join(".bellwether/vault")] {
            std::fs::create_dir_all(dir)?;
        }
        let called = work.path().join("docker-called");
        executable(
            &stub.join("docker"),
            &format!("#!/bin/sh\ntouch '{}'\n", called.display()),
        )?;
        let text = read(script)?.replace(
            WATCHDOG_DOCKER,
            &format!("{} run", stub.join("docker").display()),
        );
        let copy = work.path().join("script");
        std::fs::write(&copy, text)?;
        let mut command = Command::new(UNIT_SHELL);
        command
            .arg(&copy)
            .args(args)
            .current_dir(&cwd)
            .env_clear()
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
            .env("NEWSVAULT_IMAGE", "newsvault:test")
            .env("SERVICE_RESULT", "success")
            .stdin(Stdio::null());
        for (key, value) in &template {
            command.env(key, value);
        }
        command.env("NEXTCLOUD_USER", NC_USER);
        let output = command.output()?;
        seen.push((
            script,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).contains("NEWSVAULT_TEXTFILE_DIR"),
            called.exists(),
            sorted_names(&cwd)?,
        ));
    }
    let expected: Vec<_> = TEXTFILE_SCRIPTS
        .iter()
        .map(|(script, _)| (*script, Some(2), true, false, Vec::<String>::new()))
        .collect();
    assert_eq!((placeholder.starts_with('/'), seen), (false, expected));
    Ok(())
}

/// The roster fetch's bound: SEC's patience for each of its requests plus the margin, derived,
/// never fitted. Exit 8 fails it: a partial roster day is lost and `NewsvaultRosterFailed` says so.
/// Its whole argument list is read: a `--dry-run` there exits 0 daily and keeps no copy.
#[test]
fn the_roster_bound_is_each_requests_patience_plus_two_minutes_and_exit_8_fails() -> TestResult {
    let unit = read("deploy/systemd/newsvault-roster.service")?;
    let patience = TimeDelta::from_std(edgar_press::sec_transport().timeouts.global)?;
    let args = container_args(&unit);
    assert_eq!(
        (
            integers(&unit_values(&unit, "TimeoutStartSec")),
            unit_values(&unit, "SuccessExitStatus").len(),
            unit_values(&unit, "Restart").len(),
            usize::try_from(ROSTER_REQUESTS)? == newsvault::cmd_roster::FILES.len() + 1,
            args,
        ),
        (
            vec![(patience * ROSTER_REQUESTS + TIMEOUT_MARGIN).num_seconds()],
            0,
            0,
            true,
            ["roster", "fetch", "--non-interactive", "--format", "json"]
                .map(str::to_owned)
                .to_vec(),
        )
    );
    Ok(())
}

/// The roster runs once a day inside a gap the collect slots leave, as the backup does, never in
/// the backup's own window, never run late.
#[test]
fn the_roster_runs_once_a_day_inside_a_gap_the_collect_runs_and_the_backup_leave() -> TestResult {
    let timer = read("deploy/systemd/newsvault-roster.timer")?;
    let bound = integers(&unit_values(
        &read("deploy/systemd/newsvault-roster.service")?,
        "TimeoutStartSec",
    ));
    let backup_at = daily_times(&read("deploy/systemd/newsvault-backup.timer")?);
    let backup_bound = integers(&unit_values(
        &read("deploy/systemd/newsvault-backup.service")?,
        "TimeoutStartSec",
    ));
    let gaps = gaps(derived_timeout()? + DEFAULT_TIMEOUT_STOP_S);
    let mut fitting = Vec::new();
    for (at, bound) in daily_times(&timer).iter().flatten().zip(&bound) {
        let end = at + bound + DEFAULT_TIMEOUT_STOP_S;
        let in_gap = gaps.iter().any(|(from, next)| *from <= *at && end <= *next);
        let meets_backup = backup_at
            .iter()
            .flatten()
            .zip(&backup_bound)
            .any(|(start, run)| *at < start + run + DEFAULT_TIMEOUT_STOP_S && *start < end);
        fitting.push((in_gap, meets_backup));
    }
    assert_eq!(
        (
            fitting,
            unit_values(&timer, "AccuracySec"),
            unit_values(&timer, "RandomizedDelaySec").len(),
            unit_values(&timer, "Persistent").len(),
        ),
        (vec![(true, false)], vec!["1s".to_owned()], 0, 0)
    );
    Ok(())
}

/// One daily interval plus one run's bound: the stamp is written when a run ends, so the longest
/// quiet stretch between two runs that both ran is a day plus one run's bound.
#[test]
fn the_roster_missed_threshold_is_one_daily_interval_plus_one_runs_bound() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let bound = integers(&unit_values(
        &read("deploy/systemd/newsvault-roster.service")?,
        "TimeoutStartSec",
    ));
    assert_eq!(
        (
            threshold(&rules, "NewsvaultRosterMissed"),
            daily_times(&read("deploy/systemd/newsvault-roster.timer")?).len(),
        ),
        (
            bound
                .first()
                .map(|bound| DAY_S + bound + DEFAULT_TIMEOUT_STOP_S),
            1
        )
    );
    Ok(())
}

/// A shell constant `name=<integer>` at the start of a script's line.
fn script_constant(script: &str, name: &str) -> Option<i64> {
    let prefix = format!("{name}=");
    script
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.parse().ok())
}

/// The shadow timer's firings in a day, as seconds after 00:00 UTC: one `*-*-* *:MM,MM:00 UTC`.
fn shadow_firings(timer: &str) -> Vec<i64> {
    let minutes: Vec<i64> = match unit_values(timer, "OnCalendar").as_slice() {
        [line] => line
            .strip_prefix("*-*-* *:")
            .and_then(|rest| rest.strip_suffix(":00 UTC"))
            .map(|list| list.split(',').filter_map(|m| m.parse().ok()).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    (0..24)
        .flat_map(|hour| minutes.iter().map(move |minute| hour * 3_600 + minute * 60))
        .collect()
}

/// The cut holds the store lock the collect, backup and roster units take without waiting, so it
/// is released (the cut's bound plus its kill grace, 210 s) before the nearest of their starts
/// (01:20's roster, 240 s after 01:16). No firing falls inside a 30-minute slot's collect window
/// (slot + 15 min); 8 a day fall inside a 90-minute slot's (slot + 45 min) and exit 7 if that
/// collect is still running.
#[test]
fn the_shadow_fires_every_30_minutes_and_frees_the_store_lock_before_any_other_start() -> TestResult
{
    let timer = read("deploy/systemd/modelgate-shadow.timer")?;
    let script = read("deploy/systemd/modelgate-shadow")?;
    let firings = shadow_firings(&timer);
    let held = script_constant(&script, "cut_bound")
        .zip(script_constant(&script, "cut_kill"))
        .map(|(bound, kill)| bound + kill);
    let slots: Vec<(i64, i64)> = SLOTS
        .iter()
        .map(|slot| {
            let start = i64::from(slot.hour) * 3_600 + i64::from(slot.minute) * 60;
            (start, slot.period_minutes * 60 / 2)
        })
        .collect();
    let mut starts: Vec<i64> = slots.iter().map(|(start, _)| *start).collect();
    for timer in ["newsvault-backup.timer", "newsvault-roster.timer"] {
        starts.extend(
            daily_times(&read(&format!("deploy/systemd/{timer}"))?)
                .iter()
                .flatten(),
        );
    }
    let nearest = firings
        .iter()
        .flat_map(|fired| {
            starts
                .iter()
                .map(move |start| (start - fired).rem_euclid(DAY_S))
        })
        .min();
    let (mut in_peak_window, mut in_off_peak_window) = (0, 0);
    for fired in &firings {
        let running = slots.iter().find(|(start, half)| {
            let since = (fired - start).rem_euclid(DAY_S);
            since < *half
        });
        match running {
            Some((_, half)) if *half == SHADOW_INTERVAL_S / 2 => in_peak_window += 1,
            Some(_) => in_off_peak_window += 1,
            None => {}
        }
    }
    let spaced = firings
        .windows(2)
        .all(|pair| matches!(pair, [early, late] if late - early == SHADOW_INTERVAL_S));
    assert_eq!(
        (
            firings.len(),
            spaced,
            in_peak_window,
            in_off_peak_window,
            nearest,
            held,
            nearest > held,
            unit_values(&timer, "AccuracySec"),
            unit_values(&timer, "RandomizedDelaySec").len(),
            unit_values(&timer, "Persistent").len(),
            unit_values(&timer, "WantedBy"),
        ),
        (
            48,
            true,
            0,
            8,
            Some(240),
            Some(210),
            true,
            vec!["1s".to_owned()],
            0,
            0,
            vec!["timers.target".to_owned()]
        )
    );
    Ok(())
}

/// Exit 7 leaves the unit successful, so only `ModelgateShadowFailed`'s exits fail it; a hung
/// run is stopped, its stop timeout included, before the next firing, which an active unit would
/// swallow; a timed-out run's two containers are removed by name.
#[test]
fn the_shadow_unit_succeeds_on_exit_7_records_its_result_and_ends_before_its_next_firing(
) -> TestResult {
    let unit = read("deploy/systemd/modelgate-shadow.service")?;
    let ends_in_time: Vec<bool> = integers(&unit_values(&unit, "TimeoutStartSec"))
        .iter()
        .map(|start| start + DEFAULT_TIMEOUT_STOP_S < SHADOW_INTERVAL_S)
        .collect();
    assert_eq!(
        (
            unit_values(&unit, "ExecStart"),
            unit_values(&unit, "ExecStartPre"),
            unit_values(&unit, "ExecStopPost"),
            unit_values(&unit, "SuccessExitStatus"),
            unit_values(&unit, "Restart").len(),
            unit_values(&unit, "TimeoutStopSec").len(),
            ends_in_time,
        ),
        (
            vec![format!("{SCRIPTS}/modelgate-shadow")],
            vec![NTP_CHECK.to_owned()],
            vec![
                result_line("shadow"),
                format!("-{}", removal("modelgate-shadow-cut")),
                format!("-{}", removal("modelgate-shadow")),
            ],
            vec!["7".to_owned()],
            0,
            0,
            vec![true]
        )
    );
    Ok(())
}

/// `promtool` runs the cases in CI only and counts none, so a removed case is red here: each
/// shadow alert keeps a case that expects it quiet and one that expects it firing.
#[test]
fn each_shadow_alert_has_a_quiet_and_a_firing_promtool_case() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let cases = trimmed(&read("deploy/prometheus/newsvault.rules.test.yml")?);
    let mut seen = Vec::new();
    for alert in rules
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- alert: ModelgateShadow"))
    {
        let named = format!("alertname: ModelgateShadow{alert}");
        let expectations = |quiet: bool| {
            cases
                .windows(2)
                .filter(|pair| {
                    matches!(pair, [name, expected]
                        if *name == named
                            && expected.starts_with("exp_alerts:")
                            && (expected == "exp_alerts: []") == quiet)
                })
                .count()
        };
        seen.push((
            alert.to_owned(),
            expectations(true) > 0,
            expectations(false) > 0,
        ));
    }
    let expected: Vec<_> = ["Failed", "Stale", "CostCapReached", "SpendNearTotalCap"]
        .iter()
        .map(|alert| ((*alert).to_owned(), true, true))
        .collect();
    assert_eq!(seen, expected);
    Ok(())
}

/// Two intervals plus one run's bound: the stamp is written when a run ends at exit 0, so one
/// retried exit 7 between two full-length runs stays quiet.
#[test]
fn the_shadow_staleness_threshold_is_two_intervals_plus_one_runs_bound() -> TestResult {
    let rules = read("deploy/prometheus/newsvault.rules.yml")?;
    let unit = read("deploy/systemd/modelgate-shadow.service")?;
    let bound = integers(&unit_values(&unit, "TimeoutStartSec"));
    assert_eq!(
        (threshold(&rules, "ModelgateShadowStale"), bound.len()),
        (
            bound
                .first()
                .map(|bound| 2 * SHADOW_INTERVAL_S + bound + DEFAULT_TIMEOUT_STOP_S),
            1
        )
    );
    Ok(())
}

/// One image, two binaries: the Dockerfile builds and copies `modelgate` and its question file,
/// and each image job dry-runs it as the wrapper starts it; that run happens only in CI.
#[test]
fn the_image_carries_modelgate_and_its_question_file_and_each_image_job_dry_runs_it() -> TestResult
{
    let dockerfile = trimmed(&read("Dockerfile")?);
    let image = read(".github/workflows/image.yml")?;
    let held = |line: &str| dockerfile.contains(&line.to_owned());
    let smoked: Vec<bool> = ["build", "publish"]
        .iter()
        .map(|name| {
            job(&image, name).iter().any(|line| {
                line.contains(MODELGATE_SMOKE.0)
                    && line.contains("-- /usr/local/bin/modelgate shadow run")
                    && line.contains(
                        "--questions /usr/share/bellwether/questions/ripple-shadow-0.json",
                    )
                    && line.ends_with(MODELGATE_SMOKE.1)
            })
        })
        .collect();
    let template = env_template()?;
    let questions: Vec<&str> = template
        .iter()
        .filter(|(key, _)| key == "MODELGATE_QUESTIONS")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(
        (
            held("RUN cargo build --locked --release -p newsvault -p modelgate"),
            held(
                "COPY --from=build /src/target/release/newsvault \
                 /src/target/release/modelgate /usr/local/bin/"
            ),
            held(
                "COPY services/modelgate/questions/ripple-shadow-0.json \
                 /usr/share/bellwether/questions/"
            ),
            smoked,
            questions,
        ),
        (
            true,
            true,
            true,
            vec![true, true],
            vec!["/usr/share/bellwether/questions/ripple-shadow-0.json"]
        )
    );
    Ok(())
}
