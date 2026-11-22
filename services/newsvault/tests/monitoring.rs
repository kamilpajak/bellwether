//! `deploy/monitoring/` against the rules it routes and the stack it checks (architecture.md
//! section 8 row A1): the route's matcher and interval against the rules keyed on `run_date`, and
//! `newsvault-monitoring-check` run on a copy of `deploy/` against a stub `curl` and `docker`.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn Error>>;

const CHECK: &str = "deploy/monitoring/newsvault-monitoring-check";
const ROUTE: &str = "deploy/monitoring/alertmanager.route.yml";
const RULE_FILES: &str = "deploy/monitoring/prometheus.rule_files.yml";
const RULES: &str = "deploy/prometheus/newsvault.rules.yml";
/// The shell the VPS runs the check with: Ubuntu's `/bin/sh`.
const UNIT_SHELL: &str = "/bin/dash";
/// Every tool the check and the stubs call; the check's PATH holds these and the stubs only.
const TOOLS: [&str; 10] = [
    "sha256sum",
    "date",
    "awk",
    "sed",
    "grep",
    "sort",
    "wc",
    "tr",
    "dirname",
    "cat",
];
const TOOL_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
const ALERTMANAGER_URL: &str = "http://127.0.0.1:9093";
const DAY_S: i64 = 86_400;
const HOUR_S: i64 = 3_600;
/// The watchdog timer's interval: the last read holding a day comes at most this long before the
/// two-day range drops it.
const WATCHDOG_INTERVAL_S: i64 = 3_600;
/// Values `config show` and amtool's errors carry that no output may.
const SECRETS: [&str; 2] = ["-1009876543210", "/run/secrets/bot-token-for-test"];
const RUN_DATE_ROUTE: &str = "max without(days_ago)";
/// A rule name that extends a routed one, appended to the copied rules by `Repo::ExtraAlert`.
const EXTRA_ALERT: &str = "NewsvaultHostHeldLong";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(root: &Path, relative: &str) -> Result<String, Box<dyn Error>> {
    Ok(std::fs::read_to_string(root.join(relative))?)
}

/// Each alert's name and its `expr`, in file order.
fn rules(root: &Path) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    Ok(read(root, RULES)?
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
fn route_regex(root: &Path) -> Result<String, Box<dyn Error>> {
    read(root, ROUTE)?
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
fn routed(root: &Path) -> Result<BTreeSet<String>, Box<dyn Error>> {
    Ok(route_regex(root)?
        .strip_prefix("Newsvault(")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_default()
        .split('|')
        .map(|alert| format!("Newsvault{alert}"))
        .collect())
}

/// The route's `repeat_interval` in seconds, whole hours only.
fn route_repeat_s(root: &Path) -> Result<Option<i64>, Box<dyn Error>> {
    Ok(read(root, ROUTE)?.lines().find_map(|line| {
        let hours = line
            .trim()
            .strip_prefix("repeat_interval: ")?
            .strip_suffix('h')?;
        hours.parse::<i64>().ok().map(|hours| hours * HOUR_S)
    }))
}

/// The run_date route is the rules aggregated over `days_ago`, and it repeats once their day has
/// left the watchdog's two-day range.
#[test]
fn the_route_matches_exactly_the_run_date_alerts_and_repeats_after_their_window() -> TestResult {
    let root = repo();
    let keyed: BTreeSet<String> = rules(&root)?
        .into_iter()
        .filter(|(_, expr)| expr.contains(RUN_DATE_ROUTE))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        (routed(&root)?, route_repeat_s(&root)?),
        (keyed, Some(2 * DAY_S + WATCHDOG_INTERVAL_S))
    );
    Ok(())
}

/// How `prometheus.yml` lists the rule file.
#[derive(Default, Clone, Copy, PartialEq)]
enum Entry {
    #[default]
    Listed,
    /// Listed with a trailing comment.
    Commented,
    /// The entry commented out.
    CommentedOut,
    /// Absent from `rule_files:`, present under a later top-level key.
    UnderLaterKey,
}

/// Prometheus's last config load against the installed rule file's mtime.
#[derive(Default, Clone, Copy, PartialEq)]
enum Reload {
    #[default]
    After,
    /// After, written with a `-05:00` offset.
    AfterWest,
    Before,
    /// Before, written with a `+05:30` offset.
    BeforeEast,
    Failed,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Container {
    #[default]
    Running,
    Stopped,
    Absent,
    Denied,
    DaemonDown,
}

/// An edit to the copy of `deploy/` the check runs from.
#[derive(Clone, Copy, PartialEq)]
enum Repo {
    MatcherRemoved,
    /// A second entry in the `rule_files` snippet.
    EntryTwice,
    RepeatTwice,
    RepeatUnparsable,
    RouteAbsent,
    /// `EXTRA_ALERT` added to the rules, outside the run_date route.
    ExtraAlert,
}

/// How the stub stack differs from the repository, if at all.
#[derive(Default, Clone, Copy)]
struct Stack {
    rules_edited: bool,
    rules_absent: bool,
    entry: Entry,
    /// Prometheus loaded every rule but this one.
    not_loaded: Option<&'static str>,
    /// Prometheus loaded the second name in place of the first.
    renamed: Option<(&'static str, &'static str)>,
    /// This rule's health is the second value.
    health: Option<(&'static str, &'static str)>,
    /// The group is read from this file instead of `newsvault.rules`.
    other_file: Option<&'static str>,
    /// A second group `newsvault` answers, from `alert.rules`.
    second_group: bool,
    reload: Reload,
    /// This alert routes to the second receiver; `-` is none.
    receiver: Option<(&'static str, &'static str)>,
    /// This alert's route through the run_date child flipped.
    flipped: Option<&'static str>,
    /// The running route's `repeat_interval`: `None` is Alertmanager's `2d1h`, `Some("")` none.
    repeat: Option<&'static str>,
    /// A sibling route after the run_date one, with this `repeat_interval`.
    sibling: Option<&'static str>,
    /// A child route of the run_date one, with this `repeat_interval`.
    child: Option<&'static str>,
    /// `config show` with each sequence indented under its key, as yaml.v3 writes it.
    indented: bool,
    curl_fails: bool,
    runtime_fails: bool,
    container: Container,
    routes_fail: bool,
    show_fails: bool,
    monitoring_absent: bool,
    monitoring_relative: bool,
    repo: Option<Repo>,
    tool_missing: Option<&'static str>,
}

struct Checked {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    alerts: usize,
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

    fn verdict(&self) -> bool {
        self.stdout.contains("drift(s) found") || self.stdout.contains("no drift")
    }
}

/// Answers the rules filtered only when asked for the group `newsvault`'s alerting rules.
const CURL_STUB: &str = r#"#!/bin/sh
[ "${STUB_CURL_FAILS:-no}" = yes ] && exit 7
case "$*" in
    */api/v1/status/runtimeinfo)
        [ "${STUB_RUNTIME_FAILS:-no}" = yes ] && exit 22
        exec cat "$STUB_DIR/runtimeinfo.json" ;;
    *'rule_group[]=newsvault'*'type=alert'*/api/v1/rules) exec cat "$STUB_DIR/rules.json" ;;
    */api/v1/rules) exec cat "$STUB_DIR/rules-unfiltered.json" ;;
esac
exit 22
"#;
/// Stands in for `docker container inspect` and `docker exec <container> amtool --alertmanager.url=…
/// …`: `config show` prints `shown`; a routing test prints the tree for the alert's line in
/// `routes`, `<alert> <receiver> <yes|no>`.
const DOCKER_STUB: &str = r#"#!/bin/sh
if [ "$1 $2" = "container inspect" ]; then
    case $STUB_CONTAINER in
        running) echo true ;;
        stopped) echo false ;;
        absent) echo "Error response from daemon: No such container: $5" >&2; exit 1 ;;
        denied) echo 'permission denied while trying to connect to the Docker daemon socket' >&2; exit 1 ;;
        *) echo 'Cannot connect to the Docker daemon. Is the docker daemon running?' >&2; exit 1 ;;
    esac
    exit 0
fi
[ "$1" = exec ] && [ "$3" = amtool ] && [ "$4" = "--alertmanager.url=$STUB_AM_URL" ] || exit 125
shift 4
for arg; do
    case $arg in --config.file*) exit 1 ;; alertname=*) alert=${arg#alertname=} ;; esac
done
if [ "$1 $2" = "config show" ]; then
    [ "${STUB_SHOW_FAILS:-no}" = yes ] && exit 1
    exec cat "$STUB_DIR/shown"
fi
if [ "${STUB_ROUTES_FAIL:-no}" = yes ]; then
    echo 'chat_id: -1009876543210' >&2
    exit 1
fi
set -- $(grep "^$alert " "$STUB_DIR/routes")
receiver=${2-}
[ "$receiver" = - ] && receiver=
echo 'Matching routes:'
echo '.'
echo '└── default-route  receiver: telegram'
[ "${3-}" = yes ] && echo "    └── {alertname=~\"$STUB_REGEX\"}  receiver: $receiver"
echo
echo "$receiver"
"#;

fn executable(path: &Path, text: &str) -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// One rule of `/api/v1/rules`, firing, its alert labelled `name` as node-exporter's systemd
/// series are, with escaped quotes, commas and braces inside the strings.
fn rule_json(name: &str, health: &str, kind: &str) -> Value {
    json!({
        "state": "firing",
        "name": name,
        "query": "max without(days_ago) (x{a=\"b\",c=\"d\"}) > 0",
        "duration": 0,
        "labels": {"severity": "critical"},
        "annotations": {"summary": "{{ $labels.source }}: \"name\":\"Fake\", run date",
                        "description": "one \"quote {brace"},
        "alerts": [{"labels": {"alertname": name, "name": "newsvault-collect.service",
                               "run_date": "2026-11-21"},
                    "state": "firing", "value": "1e+00"}],
        "health": health,
        "evaluationTime": 0.0001,
        "lastEvaluation": "2026-11-22T10:00:00Z",
        "type": kind
    })
}

fn group_json(name: &str, file: &str, rules: Vec<Value>) -> Value {
    json!({"name": name, "file": format!("/etc/prometheus/{file}"), "rules": rules,
           "interval": 15, "limit": 0})
}

/// `/api/v1/rules` as Prometheus writes it, compact: filtered, and as it answers with no filter.
fn rules_json(stack: Stack, names: &[String]) -> (String, String) {
    let rules: Vec<Value> = names
        .iter()
        .filter(|name| Some(name.as_str()) != stack.not_loaded)
        .map(|name| match stack.renamed {
            Some((from, to)) if name == from => to.to_owned(),
            _ => name.clone(),
        })
        .map(|name| {
            let health = match stack.health {
                Some((rule, health)) if rule == name => health,
                _ => "ok",
            };
            rule_json(&name, health, "alerting")
        })
        .collect();
    let file = stack.other_file.unwrap_or("newsvault.rules");
    let mut groups = vec![group_json("newsvault", file, rules.clone())];
    if stack.second_group {
        groups.push(group_json("newsvault", "alert.rules", rules.clone()));
    }
    let filtered = json!({"status": "success", "data": {"groups": groups}}).to_string();
    let mut all = rules;
    all.push(rule_json("newsvault:recorded", "ok", "recording"));
    let unfiltered = json!({"status": "success", "data": {"groups": [
        group_json("host", "alert.rules", vec![rule_json("LowDiskSpace", "ok", "alerting")]),
        group_json("newsvault", file, all)
    ]}})
    .to_string();
    (filtered, unfiltered)
}

/// RFC 3339 of `secs`, with a fraction, written in the zone `offset_s` east of UTC.
fn rfc3339(secs: i64, offset_s: i64) -> String {
    let local = secs + offset_s;
    let (days, of_day) = (local.div_euclid(DAY_S), local.rem_euclid(DAY_S));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let zone = match offset_s {
        0 => "Z".to_owned(),
        _ => format!(
            "{}{:02}:{:02}",
            if offset_s < 0 { '-' } else { '+' },
            offset_s.abs() / HOUR_S,
            offset_s.abs() % HOUR_S / 60
        ),
    };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.123456789{zone}",
        of_day / HOUR_S,
        of_day % HOUR_S / 60,
        of_day % 60
    )
}

/// `/api/v1/status/runtimeinfo`, its last config load placed against `changed`.
fn runtime_json(stack: Stack, changed: i64) -> String {
    let (at, offset) = match stack.reload {
        Reload::After | Reload::Failed => (changed, 0),
        Reload::AfterWest => (changed + 1, -5 * HOUR_S),
        Reload::Before => (changed - 1, 0),
        Reload::BeforeEast => (changed - 1, 5 * HOUR_S + HOUR_S / 2),
    };
    json!({"status": "success", "data": {
        "startTime": "2026-11-01T00:00:00Z",
        "CWD": "/prometheus",
        "reloadConfigSuccess": stack.reload != Reload::Failed,
        "lastConfigTime": rfc3339(at, offset),
        "corruptionCount": 0,
        "goroutineCount": 40,
        "storageRetention": "90d"
    }})
    .to_string()
}

enum Yaml {
    Scalar(String),
    Map(Vec<(&'static str, Yaml)>),
    Seq(Vec<Yaml>),
}

fn scalar(text: &str) -> Yaml {
    Yaml::Scalar(text.to_owned())
}

/// Block YAML at `indent`; a sequence under a key sits `seq_indent` deeper than the key: 0 as
/// yaml.v2 writes it, 2 as yaml.v3 does.
fn emit(node: &Yaml, indent: usize, seq_indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match node {
        Yaml::Scalar(text) => out.push_str(&format!("{pad}{text}\n")),
        Yaml::Map(entries) => {
            for (key, value) in entries {
                match value {
                    Yaml::Scalar(text) => out.push_str(&format!("{pad}{key}: {text}\n")),
                    Yaml::Map(_) => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        emit(value, indent + 2, seq_indent, out);
                    }
                    Yaml::Seq(_) => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        emit(value, indent + seq_indent, seq_indent, out);
                    }
                }
            }
        }
        Yaml::Seq(items) => {
            for item in items {
                let mut text = String::new();
                emit(item, indent + 2, seq_indent, &mut text);
                out.push_str(&format!("{pad}- {}", &text[indent + 2..]));
            }
        }
    }
}

/// A route item: its receiver, one matcher, then `rest`.
fn route_item(receiver: &str, matcher: &str, rest: Vec<(&'static str, Yaml)>) -> Yaml {
    let mut entries = vec![
        ("receiver", scalar(receiver)),
        ("matchers", Yaml::Seq(vec![scalar(matcher)])),
    ];
    entries.extend(rest);
    Yaml::Map(entries)
}

/// The running config as `amtool config show` prints it, secrets included as Alertmanager leaves
/// them.
fn shown(stack: Stack, regex: &str) -> String {
    let mut run_date = Vec::new();
    match stack.repeat {
        None => run_date.push(("repeat_interval", scalar("2d1h"))),
        Some("") => {}
        Some(value) => run_date.push(("repeat_interval", scalar(value))),
    }
    if let Some(value) = stack.child {
        run_date.push((
            "routes",
            Yaml::Seq(vec![route_item(
                "telegram",
                "severity=\"critical\"",
                vec![("repeat_interval", scalar(value))],
            )]),
        ));
    }
    let mut routes = vec![
        route_item("blackhole", "alertname=~\"Alphalens.*\"", Vec::new()),
        route_item("telegram", &format!("alertname=~\"{regex}\""), run_date),
    ];
    if let Some(value) = stack.sibling {
        routes.push(route_item(
            "blackhole",
            "alertname=~\"Other.*\"",
            vec![("repeat_interval", scalar(value))],
        ));
    }
    let config = Yaml::Map(vec![
        ("global", Yaml::Map(vec![("resolve_timeout", scalar("5m"))])),
        (
            "route",
            Yaml::Map(vec![
                ("receiver", scalar("telegram")),
                (
                    "group_by",
                    Yaml::Seq(vec![scalar("alertname"), scalar("job")]),
                ),
                ("routes", Yaml::Seq(routes)),
                ("group_wait", scalar("30s")),
                ("repeat_interval", scalar("1d")),
            ]),
        ),
        (
            "receivers",
            Yaml::Seq(vec![
                Yaml::Map(vec![
                    ("name", scalar("telegram")),
                    (
                        "telegram_configs",
                        Yaml::Seq(vec![Yaml::Map(vec![
                            ("bot_token_file", scalar(SECRETS[1])),
                            ("chat_id", scalar(SECRETS[0])),
                        ])]),
                    ),
                ]),
                Yaml::Map(vec![("name", scalar("blackhole"))]),
            ]),
        ),
    ]);
    let mut text = String::new();
    emit(&config, 0, if stack.indented { 2 } else { 0 }, &mut text);
    text
}

/// A copy of the check and its three repository files under `root`, with `edit` applied.
fn copy_deploy(root: &Path, edit: Option<Repo>) -> TestResult {
    std::fs::create_dir_all(root.join("deploy/monitoring"))?;
    std::fs::create_dir_all(root.join("deploy/prometheus"))?;
    std::fs::copy(repo().join(CHECK), root.join(CHECK))?;
    let mut route = read(&repo(), ROUTE)?;
    let mut rules = read(&repo(), RULES)?;
    let mut entries = read(&repo(), RULE_FILES)?;
    match edit {
        Some(Repo::MatcherRemoved) => {
            route = route
                .lines()
                .filter(|line| !line.contains("alertname=~"))
                .map(|line| format!("{line}\n"))
                .collect();
        }
        Some(Repo::RepeatTwice) => route.push_str("  repeat_interval: 2d\n"),
        Some(Repo::EntryTwice) => entries.push_str("  - other.rules\n"),
        Some(Repo::RepeatUnparsable) => {
            route = route.replace("repeat_interval: 49h", "repeat_interval: 49hours");
        }
        Some(Repo::ExtraAlert) => rules.push_str(&format!(
            "\n      - alert: {EXTRA_ALERT}\n        expr: vector(1) > 2\n        labels:\n          \
             severity: warning\n"
        )),
        Some(Repo::RouteAbsent) | None => {}
    }
    if edit != Some(Repo::RouteAbsent) {
        std::fs::write(root.join(ROUTE), route)?;
    }
    std::fs::write(root.join(RULES), rules)?;
    std::fs::write(root.join(RULE_FILES), entries)?;
    Ok(())
}

/// A directory of links to `TOOLS`, less `missing`.
fn tools(dir: &Path, missing: Option<&str>) -> TestResult {
    std::fs::create_dir_all(dir)?;
    for tool in TOOLS.iter().filter(|tool| Some(**tool) != missing) {
        let found = TOOL_DIRS
            .iter()
            .map(|dir| Path::new(dir).join(tool))
            .find(|path| path.is_file())
            .ok_or_else(|| format!("no {tool} in {TOOL_DIRS:?}"))?;
        std::os::unix::fs::symlink(found, dir.join(tool))?;
    }
    Ok(())
}

fn write_monitoring(stack: Stack, root: &Path, prometheus: &Path) -> Result<i64, Box<dyn Error>> {
    std::fs::create_dir_all(prometheus)?;
    let entry = match stack.entry {
        Entry::Listed => "rule_files:\n  - 'alert.rules'\n  - 'newsvault.rules'\n",
        Entry::Commented => "rule_files:\n  - 'alert.rules'\n  - newsvault.rules # bellwether\n",
        Entry::CommentedOut => "rule_files:\n  - 'alert.rules'\n  # - 'newsvault.rules'\n",
        Entry::UnderLaterKey => "rule_files:\n  - 'alert.rules'\nlater:\n  - newsvault.rules\n",
    };
    std::fs::write(
        prometheus.join("prometheus.yml"),
        format!(
            "global:\n  scrape_interval: 15s\n{entry}scrape_configs:\n  - job_name: newsvault.rules\n"
        ),
    )?;
    if stack.rules_absent {
        return Ok(0);
    }
    let mut text = read(root, RULES)?;
    if stack.rules_edited {
        text = text.replacen("7200", "7201", 1);
    }
    let installed = prometheus.join("newsvault.rules");
    std::fs::write(&installed, text)?;
    let changed = std::fs::metadata(&installed)?
        .modified()?
        .duration_since(UNIX_EPOCH)?;
    Ok(i64::try_from(changed.as_secs())?)
}

fn check(stack: Stack) -> Result<Checked, Box<dyn Error>> {
    let work = tempfile::TempDir::new()?;
    let (root, stub, monitoring) = (
        work.path().join("repo"),
        work.path().join("stub"),
        work.path().join("monitoring"),
    );
    copy_deploy(&root, stack.repo)?;
    std::fs::create_dir_all(stub.join("bin"))?;
    tools(&stub.join("tools"), stack.tool_missing)?;
    let changed = if stack.monitoring_absent {
        0
    } else {
        write_monitoring(stack, &root, &monitoring.join("prometheus"))?
    };
    let regex = route_regex(&repo())?;
    let names: Vec<String> = rules(&root)?.into_iter().map(|(name, _)| name).collect();
    let routed = routed(&repo())?;
    let routes: String = names
        .iter()
        .map(|name| {
            let receiver = match stack.receiver {
                Some((alert, receiver)) if alert == name => receiver,
                _ => "telegram",
            };
            let matched = routed.contains(name) != (Some(name.as_str()) == stack.flipped);
            format!("{name} {receiver} {}\n", if matched { "yes" } else { "no" })
        })
        .collect();
    std::fs::write(stub.join("routes"), routes)?;
    let (filtered, unfiltered) = rules_json(stack, &names);
    std::fs::write(stub.join("rules.json"), filtered)?;
    std::fs::write(stub.join("rules-unfiltered.json"), unfiltered)?;
    std::fs::write(stub.join("runtimeinfo.json"), runtime_json(stack, changed))?;
    std::fs::write(stub.join("shown"), shown(stack, &regex))?;
    executable(&stub.join("bin/curl"), CURL_STUB)?;
    executable(&stub.join("bin/docker"), DOCKER_STUB)?;
    let yes = |set: bool| if set { "yes" } else { "no" };
    let container = match stack.container {
        Container::Running => "running",
        Container::Stopped => "stopped",
        Container::Absent => "absent",
        Container::Denied => "denied",
        Container::DaemonDown => "down",
    };
    let dir = if stack.monitoring_relative {
        PathBuf::from("monitoring")
    } else {
        monitoring
    };
    let output = Command::new(UNIT_SHELL)
        .arg(root.join(CHECK))
        .current_dir(work.path())
        .env_clear()
        .env("HOME", work.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                stub.join("bin").display(),
                stub.join("tools").display()
            ),
        )
        .env("MONITORING_DIR", dir)
        .env("STUB_DIR", &stub)
        .env("STUB_REGEX", &regex)
        .env("STUB_AM_URL", ALERTMANAGER_URL)
        .env("STUB_CONTAINER", container)
        .env("STUB_CURL_FAILS", yes(stack.curl_fails))
        .env("STUB_RUNTIME_FAILS", yes(stack.runtime_fails))
        .env("STUB_ROUTES_FAIL", yes(stack.routes_fail))
        .env("STUB_SHOW_FAILS", yes(stack.show_fails))
        .stdin(Stdio::null())
        .output()?;
    Ok(Checked {
        exit: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        alerts: names.len(),
    })
}

/// A stack equal to the repository: every check `ok`, one line per rule's route, exit 0; the
/// interval read as 2d1h, as 49h or under indented sequences; the entry with a trailing comment;
/// the last load written in another zone; a rule whose name extends a routed one.
#[test]
fn a_stack_equal_to_the_repository_passes_every_check() -> TestResult {
    let stacks = [
        Stack::default(),
        Stack {
            repeat: Some("49h"),
            ..Stack::default()
        },
        Stack {
            indented: true,
            ..Stack::default()
        },
        Stack {
            entry: Entry::Commented,
            ..Stack::default()
        },
        Stack {
            reload: Reload::AfterWest,
            ..Stack::default()
        },
        Stack {
            repo: Some(Repo::ExtraAlert),
            ..Stack::default()
        },
    ];
    let mut got = Vec::new();
    let mut expected = Vec::new();
    for stack in stacks {
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
        expected.push((Some(0), checked.alerts + 5, Vec::new(), true));
    }
    assert_eq!(got, expected);
    Ok(())
}

/// Each drift is found alone, by its own check, and the run exits 1 after every check ran.
#[test]
fn each_drift_exits_1_and_is_named_by_its_own_check() -> TestResult {
    let cases: [(Stack, &str); 26] = [
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
                entry: Entry::CommentedOut,
                ..Stack::default()
            },
            "drift rule_files",
        ),
        (
            Stack {
                entry: Entry::UnderLaterKey,
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
                health: Some(("NewsvaultCollectStale", "err")),
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
                second_group: true,
                ..Stack::default()
            },
            "drift prometheus",
        ),
        (
            Stack {
                reload: Reload::Before,
                ..Stack::default()
            },
            "drift prometheus_reload",
        ),
        (
            Stack {
                reload: Reload::BeforeEast,
                ..Stack::default()
            },
            "drift prometheus_reload",
        ),
        (
            Stack {
                reload: Reload::Failed,
                ..Stack::default()
            },
            "drift prometheus_reload",
        ),
        (
            Stack {
                receiver: Some(("NewsvaultWatchdogStale", "blackhole")),
                ..Stack::default()
            },
            "drift route NewsvaultWatchdogStale",
        ),
        (
            Stack {
                receiver: Some(("NewsvaultRunsMissed", "blackhole")),
                ..Stack::default()
            },
            "drift route NewsvaultRunsMissed",
        ),
        (
            Stack {
                receiver: Some(("NewsvaultNeverAsked", "-")),
                ..Stack::default()
            },
            "drift route NewsvaultNeverAsked",
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
        (
            Stack {
                repeat: Some("1w2d1h"),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some("2d1hx"),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some("\"2d1h\""),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some(""),
                sibling: Some("49h"),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some(""),
                sibling: Some("49h"),
                indented: true,
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some(""),
                child: Some("49h"),
                ..Stack::default()
            },
            "drift repeat_interval",
        ),
        (
            Stack {
                repeat: Some(""),
                child: Some("49h"),
                indented: true,
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

/// Two drifts are counted as two, and a drift found before a check that went unanswered still
/// exits 1.
#[test]
fn drifts_are_counted_and_a_found_drift_outranks_an_unanswered_check() -> TestResult {
    let mut got = Vec::new();
    for stack in [
        Stack {
            rules_edited: true,
            entry: Entry::CommentedOut,
            ..Stack::default()
        },
        Stack {
            rules_edited: true,
            curl_fails: true,
            ..Stack::default()
        },
    ] {
        let checked = check(stack)?;
        got.push((
            checked.exit,
            checked.drifts(),
            checked.stdout.lines().last().map(str::to_owned),
        ));
    }
    assert_eq!(
        got,
        vec![
            (
                Some(1),
                vec!["drift rules_file".to_owned(), "drift rule_files".to_owned()],
                Some("2 drift(s) found".to_owned())
            ),
            (
                Some(1),
                vec!["drift rules_file".to_owned()],
                Some("1 drift(s) found".to_owned())
            ),
        ]
    );
    Ok(())
}

/// An absent rule file is one drift, and the reload is not compared against a file that is not there.
#[test]
fn an_absent_rule_file_has_no_reload_verdict() -> TestResult {
    let checked = check(Stack {
        rules_absent: true,
        ..Stack::default()
    })?;
    assert_eq!(
        (
            checked.drifts(),
            checked.stdout.contains("prometheus_reload")
        ),
        (vec!["drift rules_file".to_owned()], false)
    );
    Ok(())
}

/// A rule not healthy is named with its health, never its error.
#[test]
fn a_drift_in_health_names_each_rule_and_its_health() -> TestResult {
    let checked = check(Stack {
        health: Some(("NewsvaultCollectStale", "err")),
        ..Stack::default()
    })?;
    assert!(checked
        .stdout
        .lines()
        .any(|line| line == "  health of NewsvaultCollectStale err"));
    Ok(())
}

/// A stack that does not answer, or has not evaluated a rule yet, exits 7; an absent input 4; a bad
/// setting or repository file 2; each before a verdict.
#[test]
fn an_unanswered_stack_exits_7_an_absent_input_4_and_a_bad_setting_2() -> TestResult {
    let cases: [(Stack, i32); 17] = [
        (
            Stack {
                curl_fails: true,
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                runtime_fails: true,
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                health: Some(("NewsvaultCollectStale", "unknown")),
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                container: Container::Stopped,
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                container: Container::DaemonDown,
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                routes_fail: true,
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                show_fails: true,
                ..Stack::default()
            },
            7,
        ),
        (
            Stack {
                container: Container::Absent,
                ..Stack::default()
            },
            4,
        ),
        (
            Stack {
                monitoring_absent: true,
                ..Stack::default()
            },
            4,
        ),
        (
            Stack {
                repo: Some(Repo::RouteAbsent),
                ..Stack::default()
            },
            4,
        ),
        (
            Stack {
                container: Container::Denied,
                ..Stack::default()
            },
            2,
        ),
        (
            Stack {
                monitoring_relative: true,
                ..Stack::default()
            },
            2,
        ),
        (
            Stack {
                repo: Some(Repo::MatcherRemoved),
                ..Stack::default()
            },
            2,
        ),
        (
            Stack {
                repo: Some(Repo::EntryTwice),
                ..Stack::default()
            },
            2,
        ),
        (
            Stack {
                repo: Some(Repo::RepeatTwice),
                ..Stack::default()
            },
            2,
        ),
        (
            Stack {
                repo: Some(Repo::RepeatUnparsable),
                ..Stack::default()
            },
            2,
        ),
        (
            Stack {
                tool_missing: Some("sha256sum"),
                ..Stack::default()
            },
            2,
        ),
    ];
    let mut got = Vec::new();
    for (stack, _) in cases {
        let checked = check(stack)?;
        got.push((checked.exit, checked.verdict()));
    }
    let expected: Vec<_> = cases.iter().map(|(_, exit)| (Some(*exit), false)).collect();
    assert_eq!(got, expected);
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
            routes_fail: true,
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
