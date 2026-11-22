//! `deploy/monitoring/` against the rules it routes and the stack it checks (architecture.md
//! section 10, V4): the route's matcher and interval against the rules keyed on `run_date`, and
//! `newsvault-monitoring-check` run against a stub `curl` and `docker`.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn Error>>;

const CHECK: &str = "deploy/monitoring/newsvault-monitoring-check";
const ROUTE: &str = "deploy/monitoring/alertmanager.route.yml";
const RULES: &str = "deploy/prometheus/newsvault.rules.yml";
/// The shell the VPS runs the check with: Ubuntu's `/bin/sh`.
const UNIT_SHELL: &str = "/bin/dash";
const DAY_S: i64 = 86_400;
/// The watchdog timer's interval: the last read holding a day comes at most this long before the
/// two-day range drops it.
const WATCHDOG_INTERVAL_S: i64 = 3_600;
/// Values `config show` and amtool's errors carry that no output may.
const SECRETS: [&str; 2] = ["-1009876543210", "/run/secrets/bot-token-for-test"];
const RUN_DATE_ROUTE: &str = "max without(days_ago)";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> Result<String, Box<dyn Error>> {
    Ok(std::fs::read_to_string(repo().join(relative))?)
}

/// Each alert's name and its `expr`, in file order.
fn rules() -> Result<Vec<(String, String)>, Box<dyn Error>> {
    Ok(read(RULES)?
        .split("- alert: ")
        .skip(1)
        .filter_map(|rule| {
            let (name, rest) = rule.split_once('\n')?;
            Some((
                name.trim().to_owned(),
                rest.split("labels:").next()?.to_owned(),
            ))
        })
        .collect())
}

/// The route's regex, inside `alertname=~"…"`.
fn route_regex() -> Result<String, Box<dyn Error>> {
    read(ROUTE)?
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("- alertname=~\"")?
                .strip_suffix('"')
                .map(str::to_owned)
        })
        .ok_or_else(|| "the route names no alertname matcher".into())
}

/// The alert names the route's regex matches: `Newsvault(A|B)` is `NewsvaultA` and `NewsvaultB`.
fn routed() -> Result<BTreeSet<String>, Box<dyn Error>> {
    Ok(route_regex()?
        .strip_prefix("Newsvault(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_default()
        .split('|')
        .map(|alert| format!("Newsvault{alert}"))
        .collect())
}

/// The route's `repeat_interval` in seconds, whole hours only.
fn route_repeat_s() -> Result<Option<i64>, Box<dyn Error>> {
    Ok(read(ROUTE)?.lines().find_map(|line| {
        let hours = line
            .trim()
            .strip_prefix("repeat_interval: ")?
            .strip_suffix('h')?;
        hours.parse::<i64>().ok().map(|hours| hours * 3_600)
    }))
}

/// The run_date route is the rules aggregated over `days_ago`, and it repeats once their day has
/// left the watchdog's two-day range.
#[test]
fn the_route_matches_exactly_the_run_date_alerts_and_repeats_after_their_window() -> TestResult {
    let routed = routed()?;
    let keyed: BTreeSet<String> = rules()?
        .into_iter()
        .filter(|(_, expr)| expr.contains(RUN_DATE_ROUTE))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        (routed, route_repeat_s()?),
        (keyed, Some(2 * DAY_S + WATCHDOG_INTERVAL_S))
    );
    Ok(())
}

/// How the stub stack differs from the repository, if at all.
#[derive(Default, Clone, Copy)]
struct Stack {
    rules_edited: bool,
    rules_absent: bool,
    entry_commented: bool,
    /// Prometheus loaded every rule but this one.
    not_loaded: Option<&'static str>,
    /// Prometheus loaded the second name in place of the first.
    renamed: Option<(&'static str, &'static str)>,
    /// This rule's health is `err`.
    unhealthy: Option<&'static str>,
    /// The group is read from this file instead of `newsvault.rules`.
    other_file: Option<&'static str>,
    /// This alert routes to the muted receiver.
    muted: Option<&'static str>,
    /// This alert's route through the run_date child flipped.
    flipped: Option<&'static str>,
    /// The running route's `repeat_interval`: `None` is Alertmanager's `2d1h`, `Some("")` none.
    repeat: Option<&'static str>,
    /// `config show` with each sequence indented under its key, as yaml.v3 writes it.
    indented: bool,
    curl_fails: bool,
    docker_fails: bool,
    routes_fail: bool,
    show_fails: bool,
    monitoring_absent: bool,
    monitoring_relative: bool,
}

struct Checked {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Checked {
    /// Each drift line up to its first `:`.
    fn drifts(&self) -> Vec<String> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with("drift "))
            .map(|line| line.split(':').next().unwrap_or_default().to_owned())
            .collect()
    }
}

const CURL_STUB: &str = r#"#!/bin/sh
[ "${STUB_CURL_FAILS:-no}" = yes ] && exit 7
exec cat "$STUB_DIR/rules.json"
"#;
/// Stands in for `docker exec <container> amtool …`: `config show` prints `shown`; a routing test
/// prints the tree for the alert's line in `routes`, `<alert> <receiver> <yes|no>`.
const DOCKER_STUB: &str = r#"#!/bin/sh
[ "$1" = exec ] && [ "$3" = amtool ] || exit 125
if [ "${STUB_DOCKER_FAILS:-no}" = yes ]; then
    echo 'chat_id: -1009876543210' >&2
    exit 1
fi
shift 3
if [ "$1 $2" = "config show" ]; then
    [ "${STUB_SHOW_FAILS:-no}" = yes ] && exit 1
    exec cat "$STUB_DIR/shown"
fi
[ "${STUB_ROUTES_FAIL:-no}" = yes ] && exit 1
for arg; do
    case $arg in alertname=*) alert=${arg#alertname=} ;; esac
done
set -- $(grep "^$alert " "$STUB_DIR/routes")
echo 'Matching routes:'
echo '.'
echo '└── default-route  receiver: telegram'
[ "${3-}" = yes ] && echo "    └── {alertname=~\"$STUB_REGEX\"}  receiver: ${2-}"
echo
echo "${2-}"
"#;

fn executable(path: &Path, text: &str) -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// `/api/v1/rules` as Prometheus writes it, compact, with escaped quotes, commas and braces inside
/// the strings and a firing alert.
fn rules_json(stack: Stack, names: &[String]) -> String {
    let rules: Vec<Value> = names
        .iter()
        .filter(|name| Some(name.as_str()) != stack.not_loaded)
        .map(|name| match stack.renamed {
            Some((from, to)) if name == from => to.to_owned(),
            _ => name.clone(),
        })
        .map(|name| {
            let health = if Some(name.as_str()) == stack.unhealthy {
                "err"
            } else {
                "ok"
            };
            json!({
                "state": "firing",
                "name": name,
                "query": "max without(days_ago) (x{a=\"b\",c=\"d\"}) > 0",
                "duration": 0,
                "labels": {"severity": "critical"},
                "annotations": {"summary": "{{ $labels.source }}: \"name\":\"Fake\", run date"},
                "alerts": [{"labels": {"alertname": name, "run_date": "2026-11-21"},
                            "state": "firing", "value": "1e+00"}],
                "health": health,
                "evaluationTime": 0.0001,
                "lastEvaluation": "2026-11-22T10:00:00Z",
                "type": "alerting"
            })
        })
        .collect();
    json!({"status": "success", "data": {"groups": [{
        "name": "newsvault",
        "file": format!("/etc/prometheus/{}", stack.other_file.unwrap_or("newsvault.rules")),
        "rules": rules,
        "interval": 15,
        "limit": 0
    }]}})
    .to_string()
}

/// The running config as `amtool config show` prints it, secrets included as Alertmanager leaves
/// them.
fn shown(stack: Stack, regex: &str) -> String {
    let repeat = match stack.repeat {
        None => "    repeat_interval: 2d1h\n".to_owned(),
        Some("") => String::new(),
        Some(value) => format!("    repeat_interval: {value}\n"),
    };
    let text = format!(
        "global:\n  resolve_timeout: 5m\nroute:\n  receiver: telegram\n  group_by:\n  - alertname\n  \
         - job\n  routes:\n  - receiver: blackhole\n    matchers:\n    - alertname=~\"Alphalens.*\"\n  \
         - receiver: telegram\n    matchers:\n    - alertname=~\"{regex}\"\n{repeat}  group_wait: 30s\n  \
         repeat_interval: 1d\nreceivers:\n- name: telegram\n  telegram_configs:\n  - bot_token_file: \
         {}\n    chat_id: {}\n- name: blackhole\n",
        SECRETS[1], SECRETS[0]
    );
    if !stack.indented {
        return text;
    }
    text.lines()
        .map(|line| match line.trim_start().strip_prefix("- ") {
            Some(_) => format!("  {line}"),
            None => line.to_owned(),
        })
        .collect::<Vec<String>>()
        .join("\n")
}

fn check(stack: Stack) -> Result<Checked, Box<dyn Error>> {
    let work = tempfile::TempDir::new()?;
    let (stub, monitoring) = (work.path().join("stub"), work.path().join("monitoring"));
    let prometheus = monitoring.join("prometheus");
    std::fs::create_dir_all(stub.join("bin"))?;
    if !stack.monitoring_absent {
        std::fs::create_dir_all(&prometheus)?;
        let entry = if stack.entry_commented {
            "  # - 'newsvault.rules'"
        } else {
            "  - 'newsvault.rules'"
        };
        std::fs::write(
            prometheus.join("prometheus.yml"),
            format!(
                "global:\n  scrape_interval: 15s\nrule_files:\n  - 'alert.rules'\n  - \
                 'alphalens.rules'\n{entry}\nscrape_configs:\n  - job_name: newsvault.rules\n"
            ),
        )?;
        if !stack.rules_absent {
            let mut text = read(RULES)?;
            if stack.rules_edited {
                text = text.replacen("7200", "7201", 1);
            }
            std::fs::write(prometheus.join("newsvault.rules"), text)?;
        }
    }
    let regex = route_regex()?;
    let names: Vec<String> = rules()?.into_iter().map(|(name, _)| name).collect();
    let routed = routed()?;
    let routes: String = names
        .iter()
        .map(|name| {
            let receiver = if Some(name.as_str()) == stack.muted {
                "blackhole"
            } else {
                "telegram"
            };
            let matched = routed.contains(name) != (Some(name.as_str()) == stack.flipped);
            format!("{name} {receiver} {}\n", if matched { "yes" } else { "no" })
        })
        .collect();
    std::fs::write(stub.join("routes"), routes)?;
    std::fs::write(stub.join("rules.json"), rules_json(stack, &names))?;
    std::fs::write(stub.join("shown"), shown(stack, &regex))?;
    executable(&stub.join("bin/curl"), CURL_STUB)?;
    executable(&stub.join("bin/docker"), DOCKER_STUB)?;
    let yes = |set: bool| if set { "yes" } else { "no" };
    let dir = if stack.monitoring_relative {
        PathBuf::from("monitoring")
    } else {
        monitoring
    };
    let output = Command::new(UNIT_SHELL)
        .arg(repo().join(CHECK))
        .current_dir(work.path())
        .env_clear()
        .env("HOME", work.path())
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                stub.join("bin").display()
            ),
        )
        .env("MONITORING_DIR", dir)
        .env("STUB_DIR", &stub)
        .env("STUB_REGEX", &regex)
        .env("STUB_CURL_FAILS", yes(stack.curl_fails))
        .env("STUB_DOCKER_FAILS", yes(stack.docker_fails))
        .env("STUB_ROUTES_FAIL", yes(stack.routes_fail))
        .env("STUB_SHOW_FAILS", yes(stack.show_fails))
        .stdin(Stdio::null())
        .output()?;
    Ok(Checked {
        exit: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// A stack equal to the repository: every check `ok`, one line per rule's route, exit 0; the
/// interval read as 2d1h, as 49h or under indented sequences.
#[test]
fn a_stack_equal_to_the_repository_passes_every_check() -> TestResult {
    let count = rules()?.len();
    let mut got = Vec::new();
    for stack in [
        Stack::default(),
        Stack {
            repeat: Some("49h"),
            ..Stack::default()
        },
        Stack {
            indented: true,
            ..Stack::default()
        },
    ] {
        let checked = check(stack)?;
        let oks = checked
            .stdout
            .lines()
            .filter(|line| line.starts_with("ok "))
            .count();
        got.push((
            checked.exit,
            oks,
            checked.drifts(),
            checked.stdout.ends_with("no drift\n"),
        ));
    }
    assert_eq!(got, vec![(Some(0), count + 4, Vec::new(), true); 3]);
    Ok(())
}

/// Each drift is found alone, by its own check, and the run exits 1 after every check ran.
#[test]
fn each_drift_exits_1_and_is_named_by_its_own_check() -> TestResult {
    let cases: [(Stack, &str); 13] = [
        (
            Stack {
                rules_edited: true,
                ..Stack::default()
            },
            "drift rules_file",
        ),
        (
            Stack {
                rules_absent: true,
                ..Stack::default()
            },
            "drift rules_file",
        ),
        (
            Stack {
                entry_commented: true,
                ..Stack::default()
            },
            "drift rule_files",
        ),
        (
            Stack {
                not_loaded: Some("NewsvaultBackupFailed"),
                ..Stack::default()
            },
            "drift prometheus",
        ),
        (
            Stack {
                renamed: Some(("NewsvaultRowsZero", "NewsvaultRowsNone")),
                ..Stack::default()
            },
            "drift prometheus",
        ),
        (
            Stack {
                unhealthy: Some("NewsvaultCollectStale"),
                ..Stack::default()
            },
            "drift prometheus",
        ),
        (
            Stack {
                other_file: Some("alphalens.rules"),
                ..Stack::default()
            },
            "drift prometheus",
        ),
        (
            Stack {
                muted: Some("NewsvaultWatchdogStale"),
                ..Stack::default()
            },
            "drift route NewsvaultWatchdogStale",
        ),
        (
            Stack {
                muted: Some("NewsvaultRunsMissed"),
                ..Stack::default()
            },
            "drift route NewsvaultRunsMissed",
        ),
        (
            Stack {
                flipped: Some("NewsvaultHostHeld"),
                ..Stack::default()
            },
            "drift route NewsvaultHostHeld",
        ),
        (
            Stack {
                flipped: Some("NewsvaultCollectFailed"),
                ..Stack::default()
            },
            "drift route NewsvaultCollectFailed",
        ),
        (
            Stack {
                repeat: Some("1d"),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some(""),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
    ];
    let mut got = Vec::new();
    for (stack, _) in cases {
        let checked = check(stack)?;
        got.push((
            checked.exit,
            checked.drifts(),
            checked.stdout.ends_with("1 drift(s) found\n"),
        ));
    }
    let expected: Vec<_> = cases
        .iter()
        .map(|(_, drift)| (Some(1), vec![(*drift).to_owned()], true))
        .collect();
    assert_eq!(got, expected);
    Ok(())
}

/// A stack that does not answer exits 7, an absent input 4, an unsafe setting 2, each before a
/// verdict.
#[test]
fn an_unanswered_stack_exits_7_an_absent_input_4_and_a_bad_setting_2() -> TestResult {
    let mut got = Vec::new();
    for stack in [
        Stack {
            curl_fails: true,
            ..Stack::default()
        },
        Stack {
            docker_fails: true,
            ..Stack::default()
        },
        Stack {
            routes_fail: true,
            ..Stack::default()
        },
        Stack {
            show_fails: true,
            ..Stack::default()
        },
        Stack {
            monitoring_absent: true,
            ..Stack::default()
        },
        Stack {
            monitoring_relative: true,
            ..Stack::default()
        },
    ] {
        let checked = check(stack)?;
        got.push((
            checked.exit,
            checked.stdout.contains("drift(s) found") || checked.stdout.contains("no drift"),
        ));
    }
    assert_eq!(
        got,
        vec![
            (Some(7), false),
            (Some(7), false),
            (Some(7), false),
            (Some(7), false),
            (Some(4), false),
            (Some(2), false)
        ]
    );
    Ok(())
}

/// The running config and amtool's errors hold the chat id and the token's path: no run, passing,
/// drifting or failing, prints either.
#[test]
fn no_run_prints_a_value_of_the_alertmanager_config() -> TestResult {
    let mut leaked = Vec::new();
    for stack in [
        Stack::default(),
        Stack {
            repeat: Some(""),
            ..Stack::default()
        },
        Stack {
            docker_fails: true,
            ..Stack::default()
        },
        Stack {
            show_fails: true,
            ..Stack::default()
        },
    ] {
        let checked = check(stack)?;
        let printed = format!("{}{}", checked.stdout, checked.stderr);
        leaked.extend(
            SECRETS
                .iter()
                .filter(|secret| printed.contains(*secret))
                .map(|secret| (*secret).to_owned()),
        );
    }
    assert_eq!(leaked, Vec::<String>::new());
    Ok(())
}
