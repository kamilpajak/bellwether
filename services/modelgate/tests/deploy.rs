//! `deploy/systemd/modelgate-shadow` run under the unit's shell against stub `docker`, `flock`
//! and `timeout` (architecture.md section 3.5, "The shadow run on the VPS"). The stub `docker`
//! hands the wrapper's own `modelgate` words to the real binary as a dry run over the committed
//! snapshot fixture, so the words and the envelope the wrapper reads are this build's.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The shell the unit runs the script with: Ubuntu's `/bin/sh`.
const UNIT_SHELL: &str = "/bin/dash";
/// Dated before the fixture's observations of 2026-07-01 to 2026-07-03.
const MODEL: &str = "example/decide-1-20260101";
const QUESTIONS: &str = "/usr/share/bellwether/questions/ripple-shadow-0.json";
const SETTINGS: [(&str, &str); 7] = [
    ("MODELGATE_MODEL", MODEL),
    ("MODELGATE_RUN_CAP_USD", "0.40"),
    ("MODELGATE_DAY_CAP_USD", "1.50"),
    ("MODELGATE_TOTAL_CAP_USD", "15"),
    ("MODELGATE_INPUT_PRICE_PER_MTOK", "0.042"),
    ("MODELGATE_GIVE_UP_AFTER_HOURS", "24"),
    ("MODELGATE_QUESTIONS", QUESTIONS),
];
const PROM: &str = "modelgate-shadow.prom";
const OLD_METRICS: &str = "modelgate_shadow_last_success_timestamp_seconds 1700000000\n";
/// A killed run's directory, which only a run holding the shadow lock may remove.
const LEFTOVER: &str = "shadow.killed";
const CAP_REACHED: &str = r#"{"cost_micro_usd":{"day":1499990,"day_cap":1500000,"planned_estimate":null,"planned_worst_case":700,"run":120,"run_cap":400000,"total":2999990,"total_cap":15000000},"exit":{"code":8,"error_code":"cost_cap_reached"}}"#;
const NO_COST: &str = r#"{"exit":{"code":0,"error_code":null}}"#;
/// Stands in for docker: records each call's words, then `----`. The cut copies the snapshot
/// fixture into the directory mounted on `/cut`. The run lists the cache, then answers as
/// `STUB_RUN` says: `real` execs the real binary on the words after `/usr/local/bin/modelgate`,
/// container paths mapped to the host's, as a dry run; `slow` sleeps; a number prints
/// `STUB_ENVELOPE` and exits with it.
const DOCKER_STUB: &str = r#"#!/bin/sh
{ printf '%s\n' "$@"; echo ----; } >>"$STUB_DIR/docker-args"
[ "$1" = run ] || exit 0
name=''; previous=''
for arg; do
    [ "$previous" = --name ] && name=$arg
    case $arg in
        type=bind,src=*,dst=/cut | type=bind,src=*,dst=/cut,readonly) cut=${arg#type=bind,src=}; cut=${cut%%,dst=*} ;;
        type=bind,src=*,dst=/modelgate*) store=${arg#type=bind,src=}; store=${store%%,dst=*} ;;
    esac
    previous=$arg
done
if [ "$name" = modelgate-shadow-cut ]; then
    [ "$STUB_CUT_EXIT" = 0 ] || exit "$STUB_CUT_EXIT"
    cp -R "$STUB_FIXTURE" "$cut/a-snapshot"
    echo '{"schema": "newsvault.cut/1"}'
    exit 0
fi
ls "$HOME/.cache/bellwether" >"$STUB_DIR/cache-during-run"
case $STUB_RUN in
    real) ;;
    slow) sleep 2; exit 0 ;;
    *) printf '%s\n' "$STUB_ENVELOPE"; exit "$STUB_RUN" ;;
esac
while [ "$1" != /usr/local/bin/modelgate ]; do shift; done
shift
case " $* " in *" --dry-run "*) ;; *) set -- "$@" --dry-run ;; esac
n=$#
for arg; do
    case $arg in
        /cut/*) arg="$cut/${arg#/cut/}" ;;
        /modelgate) arg=$store ;;
        /usr/share/bellwether/questions/*) arg=$STUB_QUESTIONS ;;
    esac
    set -- "$@" "$arg"
done
shift "$n"
exec "$STUB_MODELGATE" "$@"
"#;
/// Stands in for flock(1): records its words; `--nonblock 9` is the shadow lock, anything else
/// the store lock in front of a command. `STUB_HELD` names the lock another process holds.
const FLOCK_STUB: &str = r#"#!/bin/sh
echo "$1 $2 $3 $4 $5 $6 $7 $8" >>"$STUB_DIR/flock-args"
if [ "$*" = "--nonblock 9" ]; then [ "$STUB_HELD" = shadow ] && exit 1; exit 0; fi
[ "$STUB_HELD" = store ] && exit 7
shift 4
exec "$@"
"#;
/// Stands in for timeout(1): exits `STUB_TIMEOUT` without running the command when it is set.
const TIMEOUT_STUB: &str = r#"#!/bin/sh
[ -z "$STUB_TIMEOUT" ] || exit "$STUB_TIMEOUT"
shift 3
exec "$@"
"#;

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

/// How one run of the wrapper is set up.
#[derive(Clone, Copy)]
struct Setup {
    args: &'static [&'static str],
    cut_exit: i32,
    /// `real`, `slow`, or the exit the stub's run ends with after printing `envelope`.
    run: &'static str,
    envelope: &'static str,
    held: &'static str,
    timeout: &'static str,
    /// A setting replacing `SETTINGS`'s, `None` unsetting it.
    setting: Option<(&'static str, Option<&'static str>)>,
    key_file: bool,
}

const PLAIN: Setup = Setup {
    args: &[],
    cut_exit: 0,
    run: "real",
    envelope: "",
    held: "",
    timeout: "",
    setting: None,
    key_file: true,
};

struct Ran {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    /// Each docker call's words joined by one space.
    docker: Vec<String>,
    flock: Vec<String>,
    metric: String,
    /// `~/.cache/bellwether` while the stub's run step ran, and after the wrapper ended.
    cache_during: Vec<String>,
    cache_after: Vec<String>,
    textfile: Vec<String>,
    home: String,
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn executable(path: &Path, text: &str) -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// One run; `interrupt` sends the wrapper SIGTERM once the stub's run step has started.
fn shadow(setup: Setup, interrupt: bool) -> TestResult<Ran> {
    let work = TempDir::new()?;
    let (home, stub, textfile) = (
        work.path().join("home"),
        work.path().join("stub"),
        work.path().join("textfile"),
    );
    let cache = home.join(".cache/bellwether");
    for dir in [
        home.join(".bellwether/vault"),
        home.join(".bellwether/modelgate"),
        home.join(".config/bellwether"),
        cache.join(LEFTOVER).join("cut"),
        stub.join("bin"),
        work.path().join("run"),
        textfile.clone(),
    ] {
        std::fs::create_dir_all(dir)?;
    }
    if setup.key_file {
        std::fs::write(
            home.join(".config/bellwether/model-key"),
            "not-a-real-key\n",
        )?;
    }
    std::fs::write(textfile.join(PROM), OLD_METRICS)?;
    for (name, text) in [
        ("docker", DOCKER_STUB),
        ("flock", FLOCK_STUB),
        ("timeout", TIMEOUT_STUB),
    ] {
        executable(&stub.join("bin").join(name), text)?;
    }
    let mut command = Command::new(UNIT_SHELL);
    command
        .arg(repo("deploy/systemd/modelgate-shadow"))
        .args(setup.args)
        .current_dir(work.path())
        .env_clear()
        .env("HOME", &home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", stub.join("bin").display()),
        )
        .env("XDG_RUNTIME_DIR", work.path().join("run"))
        .env("NEWSVAULT_IMAGE", "newsvault:test")
        .env("NEWSVAULT_TEXTFILE_DIR", &textfile)
        .env("STUB_DIR", &stub)
        .env(
            "STUB_FIXTURE",
            repo("snapshot-contract/fixtures/snapshot_v1"),
        )
        .env(
            "STUB_QUESTIONS",
            repo("services/modelgate/questions/ripple-shadow-0.json"),
        )
        .env("STUB_MODELGATE", env!("CARGO_BIN_EXE_modelgate"))
        .env("STUB_CUT_EXIT", setup.cut_exit.to_string())
        .env("STUB_RUN", setup.run)
        .env("STUB_ENVELOPE", setup.envelope)
        .env("STUB_HELD", setup.held)
        .env("STUB_TIMEOUT", setup.timeout)
        .envs(SETTINGS)
        .stdin(Stdio::null());
    match setup.setting {
        Some((key, Some(value))) => command.env(key, value),
        Some((key, None)) => command.env_remove(key),
        None => &mut command,
    };
    let output: Output = if interrupt {
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        for _ in 0..200 {
            if stub.join("cache-during-run").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let killed = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()?;
        assert!(killed.success());
        child.wait_with_output()?
    } else {
        command.output()?
    };
    let mut docker = vec![String::new()];
    for word in lines(&stub.join("docker-args")) {
        match (word.as_str(), docker.last_mut()) {
            ("----", _) => docker.push(String::new()),
            (_, Some(call)) if call.is_empty() => call.push_str(&word),
            (_, Some(call)) => {
                call.push(' ');
                call.push_str(&word);
            }
            (_, None) => {}
        }
    }
    docker.pop();
    Ok(Ran {
        exit: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        docker,
        flock: lines(&stub.join("flock-args")),
        metric: std::fs::read_to_string(textfile.join(PROM)).unwrap_or_default(),
        cache_during: lines(&stub.join("cache-during-run")),
        cache_after: names(&cache),
        textfile: names(&textfile),
        home: work.path().display().to_string(),
    })
}

/// Section 8 row MG12. The cut: under the store lock without waiting and under its bound, no
/// network, the vault read-only. The run: no lock, the default bridge, the key file read-only,
/// every value from the settings. Mutations 2026-11-23, each alone: the cut's `--network none`
/// removed; the cut's flock removed; the key mount without `readonly`; `--read-only`,
/// `--cap-drop ALL` or `--security-opt no-new-privileges` removed from the run; a `--total-cap-usd`
/// of `"$MODELGATE_DAY_CAP_USD"`: red each.
#[test]
fn a_run_cuts_under_the_store_lock_then_labels_with_the_real_binary_and_writes_its_spend(
) -> TestResult {
    let ran = shadow(PLAIN, false)?;
    let home = &ran.home;
    let (uid, gid) = (id("-u")?, id("-g")?);
    let work = ran.cache_during.first().cloned().unwrap_or_default();
    let flags = format!("--pull never --user {uid}:{gid} --read-only --tmpfs /tmp:rw,size=16m");
    let cut = format!(
        "run --rm --name modelgate-shadow-cut {flags} --network none --cap-drop ALL \
         --security-opt no-new-privileges \
         --mount type=bind,src={home}/home/.bellwether/vault,dst=/vault,readonly \
         --mount type=bind,src={home}/home/.cache/bellwether/{work}/cut,dst=/cut newsvault:test \
         cut --at <at> --out /cut --non-interactive --format json"
    );
    let run = format!(
        "run --rm --name modelgate-shadow {flags} --cap-drop ALL \
         --security-opt no-new-privileges --entrypoint /usr/bin/tini \
         --mount type=bind,src={home}/home/.cache/bellwether/{work}/cut,dst=/cut,readonly \
         --network bridge --mount type=bind,src={home}/home/.bellwether/modelgate,dst=/modelgate \
         --mount type=bind,src={home}/home/.config/bellwether/model-key,dst=/run/secrets/model-key,readonly \
         --env BELLWETHER_MODEL_KEY_FILE=/run/secrets/model-key newsvault:test -- \
         /usr/local/bin/modelgate shadow run --snapshot /cut/a-snapshot --store /modelgate \
         --questions {QUESTIONS} --model {MODEL} --run-cap-usd 0.40 --day-cap-usd 1.50 \
         --total-cap-usd 15 --input-price-per-mtok 0.042 --give-up-after-hours 24 \
         --non-interactive --format json"
    );
    let at_masked: Vec<String> = ran
        .docker
        .iter()
        .map(|call| match call.split_once(" --at ") {
            Some((head, tail)) => {
                let rest = tail.split_once(' ').map(|(_, rest)| rest);
                format!("{head} --at <at> {}", rest.unwrap_or_default())
            }
            None => call.clone(),
        })
        .collect();
    let lock = format!("{home}/run/bellwether-store.lock");
    let (stamp, body) = ran
        .metric
        .rsplit_once("modelgate_shadow_last_success_timestamp_seconds ")
        .map(|(body, stamp)| (stamp.trim().parse::<i64>().ok(), body))
        .unwrap_or_default();
    assert_eq!(
        (
            ran.exit,
            at_masked,
            ran.flock,
            body,
            stamp.is_some_and(|at| at > 1_700_000_000),
            ran.cache_during.len(),
            work.starts_with("shadow.") && work != LEFTOVER,
            ran.cache_after,
            ran.textfile,
            ran.stdout.contains("\"schema\":\"modelgate.shadow_run/1\""),
        ),
        (
            Some(0),
            vec![cut, run],
            vec![
                "--nonblock 9      ".to_owned(),
                format!("--nonblock --conflict-exit-code 7 {lock} timeout --kill-after 30 180")
            ],
            "# TYPE modelgate_shadow_spent_micro_usd gauge\n\
             modelgate_shadow_spent_micro_usd{scope=\"run\"} 0\n\
             modelgate_shadow_spent_micro_usd{scope=\"day\"} 0\n\
             modelgate_shadow_spent_micro_usd{scope=\"total\"} 0\n\
             # TYPE modelgate_shadow_cap_micro_usd gauge\n\
             modelgate_shadow_cap_micro_usd{scope=\"run\"} 400000\n\
             modelgate_shadow_cap_micro_usd{scope=\"day\"} 1500000\n\
             modelgate_shadow_cap_micro_usd{scope=\"total\"} 15000000\n\
             # TYPE modelgate_shadow_cost_cap_reached gauge\n\
             modelgate_shadow_cost_cap_reached 0\n\
             # TYPE modelgate_shadow_last_success_timestamp_seconds gauge\n",
            true,
            1,
            true,
            Vec::<String>::new(),
            vec![PROM.to_owned()],
            true
        ),
        "{}",
        ran.stderr
    );
    Ok(())
}

fn id(flag: &str) -> TestResult<String> {
    let output = Command::new("id").arg(flag).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A case's name, its setup, then the exit, the metrics, the docker calls and the cache after.
type Case = (
    &'static str,
    Setup,
    i32,
    &'static str,
    usize,
    &'static [&'static str],
);

/// Section 8 row MG13: whatever ends a run, its cut is gone, and only an envelope with its cost
/// rewrites the metrics. Mutations 2026-11-23, each alone: the `EXIT` trap removed; the cut's
/// 124 passed through; the success stamp not carried over an exit 8; an envelope without a cost
/// written as empty samples: red each.
#[test]
fn every_exit_path_removes_the_cut_and_only_an_envelope_with_its_cost_rewrites_the_metrics(
) -> TestResult {
    let reached = "# TYPE modelgate_shadow_spent_micro_usd gauge\n\
         modelgate_shadow_spent_micro_usd{scope=\"run\"} 120\n\
         modelgate_shadow_spent_micro_usd{scope=\"day\"} 1499990\n\
         modelgate_shadow_spent_micro_usd{scope=\"total\"} 2999990\n\
         # TYPE modelgate_shadow_cap_micro_usd gauge\n\
         modelgate_shadow_cap_micro_usd{scope=\"run\"} 400000\n\
         modelgate_shadow_cap_micro_usd{scope=\"day\"} 1500000\n\
         modelgate_shadow_cap_micro_usd{scope=\"total\"} 15000000\n\
         # TYPE modelgate_shadow_cost_cap_reached gauge\n\
         modelgate_shadow_cost_cap_reached 1\n\
         # TYPE modelgate_shadow_last_success_timestamp_seconds gauge\n\
         modelgate_shadow_last_success_timestamp_seconds 1700000000\n";
    let removal = "rm --force modelgate-shadow-cut";
    let cases: [Case; 11] = [
        (
            "the store lock held",
            Setup {
                held: "store",
                ..PLAIN
            },
            7,
            OLD_METRICS,
            0,
            &[],
        ),
        (
            "the shadow lock held",
            Setup {
                held: "shadow",
                ..PLAIN
            },
            7,
            OLD_METRICS,
            0,
            &[LEFTOVER],
        ),
        (
            "the cut's lease_held",
            Setup {
                cut_exit: 7,
                ..PLAIN
            },
            7,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "the cut's input_absent",
            Setup {
                cut_exit: 4,
                ..PLAIN
            },
            4,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "the cut past its bound",
            Setup {
                timeout: "124",
                ..PLAIN
            },
            7,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "the cut killed",
            Setup {
                timeout: "137",
                ..PLAIN
            },
            7,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "vendor_unreachable",
            Setup { run: "7", ..PLAIN },
            7,
            OLD_METRICS,
            2,
            &[],
        ),
        (
            "a refused setting",
            Setup { run: "2", ..PLAIN },
            2,
            OLD_METRICS,
            2,
            &[],
        ),
        (
            "a cap reached",
            Setup {
                run: "8",
                envelope: CAP_REACHED,
                ..PLAIN
            },
            8,
            reached,
            2,
            &[],
        ),
        (
            "exit 8, no cost",
            Setup {
                run: "8",
                envelope: NO_COST,
                ..PLAIN
            },
            8,
            OLD_METRICS,
            2,
            &[],
        ),
        (
            "exit 0, no cost",
            Setup {
                run: "0",
                envelope: NO_COST,
                ..PLAIN
            },
            2,
            OLD_METRICS,
            2,
            &[],
        ),
    ];
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (name, setup, exit, metric, calls, left) in cases {
        let ran = shadow(setup, false)?;
        let timed_out = !setup.timeout.is_empty();
        seen.push((
            name,
            ran.exit,
            ran.metric,
            ran.docker.len(),
            ran.docker.last().is_some_and(|call| call == removal),
            ran.cache_after,
            ran.textfile,
        ));
        expected.push((
            name,
            Some(exit),
            metric.to_owned(),
            calls,
            timed_out,
            left.iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            vec![PROM.to_owned()],
        ));
    }
    assert_eq!(seen, expected);
    Ok(())
}

/// A terminated run removes its cut too, and the leftover of a killed one went when it started.
/// Mutation 2026-11-23, the `INT TERM` trap removed: red (the shell dies of the signal).
#[test]
fn a_terminated_run_exits_130_and_leaves_no_cut() -> TestResult {
    let ran = shadow(
        Setup {
            run: "slow",
            ..PLAIN
        },
        true,
    )?;
    assert_eq!(
        (ran.exit, ran.cache_after, ran.metric.as_str()),
        (Some(130), Vec::<String>::new(), OLD_METRICS)
    );
    Ok(())
}

/// The runbook's dry run: no network, no key file, the store read-only, nothing written.
/// Mutations 2026-11-23: the dry branch given the live one's mounts; the metrics written for a
/// dry run: red each.
#[test]
fn a_dry_run_has_no_network_and_no_key_and_writes_no_metric() -> TestResult {
    let setup = Setup {
        args: &["--dry-run", "--from-seq", "0"],
        key_file: false,
        ..PLAIN
    };
    let ran = shadow(setup, false)?;
    let run = ran.docker.last().cloned().unwrap_or_default();
    assert_eq!(
        (
            ran.exit,
            run.contains(" --network none --mount type=bind,src=")
                && run
                    .contains("/home/.bellwether/modelgate,dst=/modelgate,readonly newsvault:test"),
            run.contains("model-key") || run.contains("bridge"),
            run.ends_with("--non-interactive --format json --dry-run --from-seq 0"),
            ran.stdout.contains("\"dry_run\":true") && ran.stdout.contains("\"start_seq\":0"),
            ran.metric.as_str(),
            ran.cache_after,
        ),
        (
            Some(0),
            true,
            false,
            true,
            true,
            OLD_METRICS,
            Vec::<String>::new()
        ),
        "{}{}",
        ran.stdout,
        ran.stderr
    );
    Ok(())
}

/// The template's `KEY=value` lines for the shadow's settings.
fn template() -> TestResult<Vec<(String, String)>> {
    Ok(
        std::fs::read_to_string(repo("deploy/systemd/newsvault.env"))?
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| key.starts_with("MODELGATE_"))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    )
}

/// Section 8 row MG14: no cap, price, model or age limit has a default, each template
/// placeholder is refused by name, a total above the owner's 15 USD is refused, and no argument
/// can restate a setting; all before any docker call. Mutations 2026-11-23, each alone: a
/// `${MODELGATE_RUN_CAP_USD:-1}` default; the ceiling test removed; the argument check removed:
/// red each.
#[test]
fn each_setting_is_required_and_the_owners_ceiling_and_the_argument_list_are_held() -> TestResult {
    let template = template()?;
    let mut settings: Vec<(&'static str, Option<&'static str>)> = Vec::new();
    for (key, _) in SETTINGS {
        settings.push((key, None));
    }
    settings.extend([
        ("MODELGATE_TOTAL_CAP_USD", Some("15.01")),
        ("MODELGATE_TOTAL_CAP_USD", Some("1e1")),
        ("MODELGATE_DAY_CAP_USD", Some("1.5.0")),
        ("MODELGATE_MODEL", Some("-x")),
        ("MODELGATE_QUESTIONS", Some("questions.json")),
    ]);
    let mut seen = Vec::new();
    for (key, value) in &settings {
        let ran = shadow(
            Setup {
                setting: Some((key, *value)),
                ..PLAIN
            },
            false,
        )?;
        seen.push((ran.exit, ran.stderr.contains(key), ran.docker.len()));
    }
    let mut placeholders = 0;
    for (key, value) in &template {
        if !value.starts_with('<') {
            continue;
        }
        placeholders += 1;
        let mut command = Command::new(UNIT_SHELL);
        let output = command
            .arg(repo("deploy/systemd/modelgate-shadow"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .env("NEWSVAULT_IMAGE", "newsvault:test")
            .env("NEWSVAULT_TEXTFILE_DIR", "/nonexistent")
            .envs(SETTINGS)
            .env(key, value)
            .output()?;
        let named = String::from_utf8_lossy(&output.stderr).contains(key.as_str());
        seen.push((output.status.code(), named, 0));
    }
    let arguments: [&[&str]; 6] = [
        &["--total-cap-usd", "99"],
        &["--from-seq", "0"],
        &["--limit"],
        &["--limit", "x"],
        &["--dry-run", "--sample-only"],
        &["--ack-cost-model", "a b"],
    ];
    for args in arguments {
        let ran = shadow(Setup { args, ..PLAIN }, false)?;
        seen.push((ran.exit, true, ran.docker.len()));
    }
    let no_key = shadow(
        Setup {
            key_file: false,
            ..PLAIN
        },
        false,
    )?;
    assert_eq!(
        (
            seen.len(),
            seen.iter()
                .filter(|case| **case != (Some(2), true, 0))
                .count(),
            placeholders,
            template.len(),
            (no_key.exit, no_key.docker.len()),
        ),
        (settings.len() + 6 + 6, 0, 6, SETTINGS.len(), (Some(4), 0)),
        "{seen:?}"
    );
    Ok(())
}
