//! `deploy/systemd/modelgate-shadow` run under the unit's shell against stub `docker`, `flock`
//! and `timeout` (architecture.md section 3.5, "The shadow run on the VPS"). The stub `docker`
//! hands the wrapper's own `modelgate` words to the real binary as a dry run over the committed
//! snapshot fixture, so the words and the envelope the wrapper reads are this build's.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use modelgate::cmd_embed::COMPARE_SECONDS_PAST_DEADLINE;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The shell the unit runs the script with: Ubuntu's `/bin/sh`.
const UNIT_SHELL: &str = "/bin/dash";
/// Dated before the fixture's observations of 2026-07-01 to 2026-07-03.
const MODEL: &str = "example/decide-1-20260101";
const QUESTIONS: &str = "/usr/share/bellwether/questions/ripple-shadow-1.json";
const SETTINGS: [(&str, &str); 8] = [
    ("MODELGATE_MODEL", MODEL),
    ("MODELGATE_RUN_CAP_USD", "0.40"),
    ("MODELGATE_DAY_CAP_USD", "1.50"),
    ("MODELGATE_TOTAL_CAP_USD", "15"),
    ("MODELGATE_INPUT_PRICE_PER_MTOK", "0.042"),
    ("MODELGATE_GIVE_UP_AFTER_HOURS", "24"),
    ("MODELGATE_LIMIT", "21"),
    ("MODELGATE_QUESTIONS", QUESTIONS),
];
/// The embed step's nine: all or none. Without them the script runs as it did before the step.
const EMBED_SETTINGS: [(&str, &str); 9] = [
    ("MODELGATE_EMBED_MODEL", "example/embed-1"),
    ("MODELGATE_EMBED_PROVIDER", "example-host"),
    ("MODELGATE_EMBED_QUANTIZATION", "bf16"),
    ("MODELGATE_EMBED_EPOCH", "1"),
    ("MODELGATE_EMBED_PRICE_PER_MTOK", "0.01"),
    ("MODELGATE_EMBED_LIMIT", "200"),
    ("MODELGATE_EMBED_RUN_CAP_USD", "0.05"),
    ("MODELGATE_EMBED_DAY_CAP_USD", "0.50"),
    ("MODELGATE_EMBED_TOTAL_CAP_USD", "3"),
];
/// A decide total that leaves the embed total its 3 USD under the owner's 15.
const DECIDE_TOTAL_BESIDE_EMBED: (&str, Option<&str>) = ("MODELGATE_TOTAL_CAP_USD", Some("12"));
const PROM: &str = "modelgate-shadow.prom";
const EMBED_PROM: &str = "modelgate-embed.prom";
const OLD_METRICS: &str = "modelgate_shadow_last_success_timestamp_seconds 1700000000\n";
/// A killed run's directory, which only a run holding the shadow lock may remove.
const LEFTOVER: &str = "shadow.killed";
const CAP_REACHED: &str = r#"{"cost_micro_usd":{"day":1499990,"day_cap":1500000,"planned_estimate":null,"planned_worst_case":700,"run":120,"run_cap":400000,"total":2999990,"total_cap":15000000},"exit":{"code":8,"error_code":"cost_cap_reached"}}"#;
const NO_COST: &str = r#"{"exit":{"code":0,"error_code":null}}"#;
/// Stands in for docker: records each call's words, then `----`. The cut copies the snapshot
/// fixture into the directory mounted on `/cut`, as many times as `STUB_CUT_DIRS` has words. The
/// run lists the cache, then answers as `STUB_RUN` says: `real` execs the real binary on the
/// words after `/usr/local/bin/modelgate`, container paths mapped to the host's, as a dry run;
/// `slow` outlasts the test, which signals the process group; a number prints `STUB_ENVELOPE`
/// and exits with it. The embed container answers as `STUB_EMBED_RUN` says when that is set.
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
    for dir in $STUB_CUT_DIRS; do cp -R "$STUB_FIXTURE" "$cut/$dir"; done
    echo '{"schema": "newsvault.cut/1"}'
    exit 0
fi
ls "$HOME/.cache/bellwether" >"$STUB_DIR/cache-during-run"
if [ "$name" = modelgate-embed ] && [ -n "$STUB_EMBED_RUN" ]; then
    printf '%s\n' "$STUB_EMBED_ENVELOPE"; exit "$STUB_EMBED_RUN"
fi
case $STUB_RUN in
    real) ;;
    slow) sleep 60; exit 0 ;;
    *) printf '%s\n' "$STUB_ENVELOPE"; exit "$STUB_RUN" ;;
esac
while [ "$1" != /usr/local/bin/modelgate ]; do shift; done
shift
case "$name: $* " in modelgate-dedup:* | *" --dry-run "*) ;; *) set -- "$@" --dry-run ;; esac
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
echo "$1 $2 $3" >>"$STUB_DIR/timeout-args"
[ -z "$STUB_TIMEOUT" ] || exit "$STUB_TIMEOUT"
shift 3
exec "$@"
"#;
/// Stands in for date(1) where the wrapper times its cut: with `STUB_CUT_SECONDS` set, the first
/// `date +%s` answers 1000 and the second that many seconds more. Any other call is the real one.
const DATE_STUB: &str = r#"#!/bin/sh
if [ "$*" != +%s ] || [ -z "$STUB_CUT_SECONDS" ]; then exec /bin/date "$@"; fi
if [ -f "$STUB_DIR/clock" ]; then echo $((1000 + STUB_CUT_SECONDS)); exit 0; fi
: >"$STUB_DIR/clock"
echo 1000
"#;
/// A zone that is not UTC and needs no zone file, so a cutoff taken without `-u` is 9 h off.
const HOST_ZONE: &str = "JST-9";
/// `date`'s format for the cutoff; two such strings order as their instants do.
const CUTOFF_FORMAT: &str = "+%Y-%m-%dT%H:%M:%SZ";

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
    /// The directory names the stub's cut leaves.
    cut_dirs: &'static str,
    /// The seconds the stub clock gives the cut; empty for the real clock.
    cut_seconds: &'static str,
    /// `real`, `slow`, or the exit the stub's run ends with after printing `envelope`.
    run: &'static str,
    envelope: &'static str,
    held: &'static str,
    timeout: &'static str,
    /// A setting replacing `SETTINGS`'s, `None` unsetting it.
    setting: Option<(&'static str, Option<&'static str>)>,
    key_file: bool,
    /// A killed run's directory in the cache; `false` leaves `~/.cache/bellwether` absent.
    leftover: bool,
    /// A directory under `~/.bellwether` left out.
    missing: &'static str,
    /// Whether `EMBED_SETTINGS` are set, and what the embed container answers: empty for the
    /// real binary's dry run, else the exit it ends with after printing `embed_envelope`.
    embed: bool,
    embed_run: &'static str,
    embed_envelope: &'static str,
    /// A second setting, replacing or unsetting one after `setting`.
    also: Option<(&'static str, Option<&'static str>)>,
}

const PLAIN: Setup = Setup {
    args: &[],
    cut_exit: 0,
    cut_dirs: "a-snapshot",
    cut_seconds: "",
    run: "real",
    envelope: "",
    held: "",
    timeout: "",
    setting: None,
    key_file: true,
    leftover: true,
    missing: "",
    embed: false,
    embed_run: "",
    embed_envelope: "",
    also: None,
};
/// `PLAIN` with the embed step on, the cut timed by the stub clock: it starts at 1000 and takes 5 s.
const EMBEDDING: Setup = Setup {
    embed: true,
    cut_seconds: "5",
    setting: Some(DECIDE_TOTAL_BESIDE_EMBED),
    ..PLAIN
};

struct Ran {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    /// Each docker call's words joined by one space.
    docker: Vec<String>,
    flock: Vec<String>,
    timeouts: Vec<String>,
    metric: String,
    /// `modelgate-embed.prom`, empty when the run wrote none.
    embed_metric: String,
    /// `~/.cache/bellwether` while the stub's run step ran, and after the wrapper ended.
    cache_during: Vec<String>,
    cache_after: Vec<String>,
    textfile: Vec<String>,
    /// `XDG_RUNTIME_DIR`'s names after the run: the stub `flock` creates none.
    runtime: Vec<String>,
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

/// `date -u` in the cutoff's format.
fn utc_now() -> TestResult<String> {
    let output = Command::new("/bin/date")
        .args(["-u", CUTOFF_FORMAT])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// One run; `interrupt` names a signal sent to the wrapper's process group, as systemd's stop
/// and a terminal's do, once the stub's run step has started.
fn shadow(setup: Setup, interrupt: Option<&str>) -> TestResult<Ran> {
    use std::os::unix::process::CommandExt;
    let work = TempDir::new()?;
    let (home, stub, textfile) = (
        work.path().join("home"),
        work.path().join("stub"),
        work.path().join("textfile"),
    );
    let cache = home.join(".cache/bellwether");
    let mut dirs = vec![
        home.join(".config/bellwether"),
        stub.join("bin"),
        work.path().join("run"),
        textfile.clone(),
    ];
    for name in ["vault", "modelgate"] {
        if setup.missing != name {
            dirs.push(home.join(".bellwether").join(name));
        }
    }
    if setup.leftover {
        dirs.push(cache.join(LEFTOVER).join("cut"));
    }
    for dir in dirs {
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
        ("date", DATE_STUB),
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
        .env("TZ", HOST_ZONE)
        .env("STUB_DIR", &stub)
        .env(
            "STUB_FIXTURE",
            repo("snapshot-contract/fixtures/snapshot_v1"),
        )
        .env(
            "STUB_QUESTIONS",
            repo("services/modelgate/questions/ripple-shadow-1.json"),
        )
        .env("STUB_MODELGATE", env!("CARGO_BIN_EXE_modelgate"))
        .env("STUB_CUT_EXIT", setup.cut_exit.to_string())
        .env("STUB_CUT_DIRS", setup.cut_dirs)
        .env("STUB_CUT_SECONDS", setup.cut_seconds)
        .env("STUB_RUN", setup.run)
        .env("STUB_ENVELOPE", setup.envelope)
        .env("STUB_HELD", setup.held)
        .env("STUB_TIMEOUT", setup.timeout)
        .env("STUB_EMBED_RUN", setup.embed_run)
        .env("STUB_EMBED_ENVELOPE", setup.embed_envelope)
        .envs(SETTINGS)
        .stdin(Stdio::null());
    if setup.embed {
        command.envs(EMBED_SETTINGS);
    }
    for change in [setup.setting, setup.also] {
        match change {
            Some((key, Some(value))) => command.env(key, value),
            Some((key, None)) => command.env_remove(key),
            None => &mut command,
        };
    }
    let output: Output = if let Some(signal) = interrupt {
        let child = command
            .process_group(0)
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
            .args(["-s", signal, "--", &format!("-{}", child.id())])
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
        timeouts: lines(&stub.join("timeout-args")),
        metric: std::fs::read_to_string(textfile.join(PROM)).unwrap_or_default(),
        embed_metric: std::fs::read_to_string(textfile.join(EMBED_PROM)).unwrap_or_default(),
        cache_during: lines(&stub.join("cache-during-run")),
        cache_after: names(&cache),
        textfile: names(&textfile),
        runtime: names(&work.path().join("run")),
        home: work.path().display().to_string(),
    })
}

/// Section 8 row MG15. The cut: under the store lock without waiting and under its bound, no
/// network, the vault read-only, its cutoff the UTC clock's on a host in another zone. The run:
/// no lock, the default bridge, the key file read-only, every value from the settings. Mutations
/// 2026-11-23, each alone: the cut's `--network none` removed; the cut's flock removed; the key
/// mount without `readonly`; `--read-only`, `--cap-drop ALL` or `--security-opt
/// no-new-privileges` removed from the run; a `--total-cap-usd` of `"$MODELGATE_DAY_CAP_USD"`;
/// the cutoff a constant, the local clock's or a day old; the shadow lock opened on the store
/// lock's file or not opened; the timer run without `--limit`: red each.
#[test]
fn a_run_cuts_under_the_store_lock_then_labels_with_the_real_binary_and_writes_its_spend(
) -> TestResult {
    let before = utc_now()?;
    let ran = shadow(PLAIN, None)?;
    let after = utc_now()?;
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
         --non-interactive --format json --limit 21"
    );
    let cutoffs: Vec<&str> = ran
        .docker
        .iter()
        .filter_map(|call| call.split_once(" --at ")?.1.split(' ').next())
        .collect();
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
            matches!(cutoffs.as_slice(), [at] if at.len() == before.len() && **at >= *before && **at <= *after),
            ran.runtime,
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
            true,
            true,
            vec!["bellwether-shadow.lock".to_owned()]
        ),
        "{cutoffs:?} in {before}..{after}\n{}",
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

/// Section 8 row MG16: whatever ends a run, its cut is gone, and only an envelope with its cost
/// rewrites the metrics. Mutations 2026-11-23, each alone: the `EXIT` trap removed; the cut's
/// 124 passed through; the success stamp not carried over an exit 8; an envelope without a cost
/// written as empty samples; a 137 before the bound read as the bound; the directory count
/// `-ge 1` or `-le 1`: red each.
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
    let cases: [Case; 14] = [
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
            "the cut killed at its bound",
            Setup {
                timeout: "137",
                cut_seconds: "180",
                ..PLAIN
            },
            7,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "the cut killed before its bound",
            Setup {
                timeout: "137",
                cut_seconds: "179",
                ..PLAIN
            },
            137,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "a cut that left nothing",
            Setup {
                cut_dirs: "",
                ..PLAIN
            },
            2,
            OLD_METRICS,
            1,
            &[],
        ),
        (
            "a cut that left two directories",
            Setup {
                cut_dirs: "a-snapshot b-snapshot",
                ..PLAIN
            },
            2,
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
        let ran = shadow(setup, None)?;
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

/// A signalled run removes its cut too, and the leftover of a killed one went when it started.
/// HUP is a dropped SSH session's and INT a terminal's, in the runbook's runs by hand. Mutations
/// 2026-11-23, each alone: the signal trap removed; each of its five names dropped: red each.
#[test]
fn a_signalled_run_exits_130_and_leaves_no_cut() -> TestResult {
    const SIGNALS: [&str; 5] = ["TERM", "INT", "HUP", "QUIT", "PIPE"];
    let mut seen = Vec::new();
    for signal in SIGNALS {
        let ran = shadow(
            Setup {
                run: "slow",
                ..PLAIN
            },
            Some(signal),
        )?;
        seen.push((signal, ran.exit, ran.cache_after, ran.metric));
    }
    let expected: Vec<_> = SIGNALS
        .iter()
        .map(|signal| {
            (
                *signal,
                Some(130),
                Vec::<String>::new(),
                OLD_METRICS.to_owned(),
            )
        })
        .collect();
    assert_eq!(seen, expected);
    Ok(())
}

/// The runbook's dry run: no network, no key file, the store read-only, nothing written, the
/// whole archive planned (no `--limit`), from a host with no `~/.cache/bellwether` yet.
/// Mutations 2026-11-23: the dry branch given the live one's mounts; the metrics written for a
/// dry run; `MODELGATE_LIMIT` passed to a dry run; `mkdir -p` of the cache removed: red each.
#[test]
fn a_dry_run_has_no_network_and_no_key_and_writes_no_metric() -> TestResult {
    let setup = Setup {
        args: &["--dry-run", "--from-seq", "0"],
        key_file: false,
        leftover: false,
        ..PLAIN
    };
    let ran = shadow(setup, None)?;
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

/// A run by hand sends what its own `--limit` says, once. Mutation 2026-11-23, the setting's
/// limit added beside it: red.
#[test]
fn a_limit_given_by_hand_replaces_the_settings() -> TestResult {
    let ran = shadow(
        Setup {
            args: &["--limit", "1"],
            ..PLAIN
        },
        None,
    )?;
    let run = ran.docker.last().cloned().unwrap_or_default();
    assert_eq!(
        (ran.exit, run.ends_with(" --format json --limit 1")),
        (Some(0), true),
        "{run}"
    );
    Ok(())
}

/// The template's `KEY=value` lines for the shadow's settings, or for its embed step's.
fn template(embed: bool) -> TestResult<Vec<(String, String)>> {
    Ok(
        std::fs::read_to_string(repo("deploy/systemd/newsvault.env"))?
            .lines()
            .filter_map(|line| line.split_once('='))
            .filter(|(key, _)| key.starts_with("MODELGATE_"))
            .filter(|(key, _)| key.starts_with("MODELGATE_EMBED_") == embed)
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    )
}

/// Section 8 row MG17: no cap, price, model, age limit or request limit has a default, each
/// template placeholder is refused by name, a total above the owner's 15 USD is refused, and no
/// argument can restate a setting; all before any docker call. Mutations 2026-11-23, each alone:
/// a `${MODELGATE_RUN_CAP_USD:-1}` default; the ceiling test removed; the argument check removed;
/// the leading-zero refusal removed; the model's, the question path's or the hours' characters
/// widened; a default image; either directory check or the `XDG_RUNTIME_DIR` check removed: red
/// each.
#[test]
fn each_setting_is_required_and_the_owners_ceiling_and_the_argument_list_are_held() -> TestResult {
    let template = template(false)?;
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
        ("MODELGATE_TOTAL_CAP_USD", Some("015")),
        ("MODELGATE_RUN_CAP_USD", Some("00.40")),
        ("MODELGATE_QUESTIONS", Some("/a b.json")),
        ("MODELGATE_MODEL", Some("a/b:/c-20260101")),
        ("MODELGATE_MODEL", Some("a/b c-20260101")),
        ("MODELGATE_GIVE_UP_AFTER_HOURS", Some("2.5")),
        ("MODELGATE_LIMIT", Some("0")),
        ("MODELGATE_LIMIT", Some("2.5")),
        ("NEWSVAULT_IMAGE", None),
        ("XDG_RUNTIME_DIR", None),
    ]);
    let mut seen = Vec::new();
    for (key, value) in &settings {
        let ran = shadow(
            Setup {
                setting: Some((key, *value)),
                ..PLAIN
            },
            None,
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
    let arguments: [&[&str]; 13] = [
        &["--total-cap-usd", "99"],
        &["--from-seq", "0"],
        &["--limit"],
        &["--limit", "x"],
        &["--dry-run", "--sample-only"],
        &["--ack-cost-model", "a b"],
        &["--embed-limit", "5"],
        &["--embed-ack-cost-model", "2026-11-23T10:16:01.000000Z"],
        &["--dedup", "everything"],
        &["--dedup", "sample", "--seed", "7"],
        &[
            "--dedup",
            "distribution",
            "--seed",
            "7",
            "--per-stratum",
            "2",
        ],
        &["--dedup", "distribution", "--dry-run"],
        &["--seed", "7", "--per-stratum", "2"],
    ];
    for args in arguments {
        let ran = shadow(Setup { args, ..PLAIN }, None)?;
        seen.push((ran.exit, true, ran.docker.len()));
    }
    let no_key = shadow(
        Setup {
            key_file: false,
            ..PLAIN
        },
        None,
    )?;
    let mut absent = Vec::new();
    for missing in ["vault", "modelgate"] {
        let ran = shadow(Setup { missing, ..PLAIN }, None)?;
        absent.push((ran.exit, ran.docker.len()));
    }
    assert_eq!(
        (
            seen.len(),
            seen.iter()
                .filter(|case| **case != (Some(2), true, 0))
                .count(),
            placeholders,
            template.len(),
            (no_key.exit, no_key.docker.len()),
            absent,
        ),
        (
            settings.len() + 7 + 13,
            0,
            7,
            SETTINGS.len(),
            (Some(4), 0),
            vec![(Some(4), 0); 2]
        ),
        "{seen:?}"
    );
    Ok(())
}

fn embed_metrics(reached: u8) -> String {
    format!(
        "# TYPE modelgate_embed_spent_micro_usd gauge\n\
         modelgate_embed_spent_micro_usd{{scope=\"run\"}} 0\n\
         modelgate_embed_spent_micro_usd{{scope=\"day\"}} 0\n\
         modelgate_embed_spent_micro_usd{{scope=\"total\"}} 0\n\
         # TYPE modelgate_embed_cap_micro_usd gauge\n\
         modelgate_embed_cap_micro_usd{{scope=\"run\"}} 50000\n\
         modelgate_embed_cap_micro_usd{{scope=\"day\"}} 500000\n\
         modelgate_embed_cap_micro_usd{{scope=\"total\"}} 3000000\n\
         # TYPE modelgate_embed_cost_cap_reached gauge\n\
         modelgate_embed_cost_cap_reached {reached}\n"
    )
}

/// Section 8 row MG25. The embed step runs after the labelling, over the same cut, with its own
/// caps, a deadline 120 s before the unit's bound counted from the cut's start, and a `timeout`
/// that ends 30 s before that bound. Mutations 2026-11-24, each alone:
/// `--total-cap-usd "$MODELGATE_TOTAL_CAP_USD"` on the embed call; the deadline from `date` at
/// the step's start; the embed metrics written into `modelgate-shadow.prom`: red each.
#[test]
fn with_its_settings_the_embed_step_follows_the_labelling_under_a_deadline_with_its_own_metrics(
) -> TestResult {
    let ran = shadow(EMBEDDING, None)?;
    let home = &ran.home;
    let (uid, gid) = (id("-u")?, id("-g")?);
    let work = ran.cache_during.first().cloned().unwrap_or_default();
    let embed = format!(
        "run --rm --name modelgate-embed --pull never --user {uid}:{gid} --read-only \
         --tmpfs /tmp:rw,size=16m --cap-drop ALL --security-opt no-new-privileges \
         --entrypoint /usr/bin/tini \
         --mount type=bind,src={home}/home/.cache/bellwether/{work}/cut,dst=/cut,readonly \
         --network bridge --mount type=bind,src={home}/home/.bellwether/modelgate,dst=/modelgate \
         --mount type=bind,src={home}/home/.config/bellwether/model-key,dst=/run/secrets/model-key,readonly \
         --env BELLWETHER_MODEL_KEY_FILE=/run/secrets/model-key newsvault:test -- \
         /usr/local/bin/modelgate embed run --snapshot /cut/a-snapshot --store /modelgate \
         --model example/embed-1 --provider example-host --quantization bf16 --epoch 1 \
         --run-cap-usd 0.05 --day-cap-usd 0.50 --total-cap-usd 3 --input-price-per-mtok 0.01 \
         --give-up-after-hours 24 --limit 200 --deadline @2380 --non-interactive --format json"
    );
    let (body, stamp) = ran
        .embed_metric
        .rsplit_once("# TYPE modelgate_embed_last_success_timestamp_seconds gauge\n")
        .unwrap_or_default();
    let stamped = stamp
        .strip_prefix("modelgate_embed_last_success_timestamp_seconds ")
        .and_then(|at| at.trim().parse::<i64>().ok());
    let order = (
        ran.stdout.find("\"schema\":\"modelgate.shadow_run/1\""),
        ran.stdout.find("\"schema\":\"modelgate.embed_run/1\""),
    );
    assert_eq!(
        (
            ran.exit,
            ran.docker.len(),
            ran.docker.get(2),
            ran.docker
                .get(1)
                .is_some_and(|run| run.contains(" --total-cap-usd 12 ")),
            ran.timeouts,
            body,
            stamped.is_some_and(|at| at > 1_700_000_000),
            ran.textfile,
            matches!(order, (Some(labelled), Some(embedded)) if labelled < embedded),
            ran.metric.contains("modelgate_embed"),
        ),
        (
            Some(0),
            3,
            Some(&embed),
            true,
            vec![
                "--kill-after 30 180".to_owned(),
                "--kill-after 10 1465".to_owned()
            ],
            embed_metrics(0).as_str(),
            true,
            vec![EMBED_PROM.to_owned(), PROM.to_owned()],
            true,
            false,
        ),
        "{}",
        ran.stderr
    );
    Ok(())
}

/// Section 8 row MG25: whatever the embed step does, the run's exit and `modelgate-shadow.prom`
/// are the labelling's, and a refused embed setting turns the step off, named, with no embed
/// container. Mutations 2026-11-24, each alone: `exit "$embed_status"` at the end; the embed
/// settings checked in a `{ }` group and not a subshell; the summed ceiling removed; its `<=`
/// made `<`; the `[ -n "$embed" ] || exit` line removed (red in the test of row MG15): red each.
#[test]
fn the_embed_step_never_changes_the_runs_exit_and_a_refused_embed_setting_only_turns_it_off(
) -> TestResult {
    let answered = |embed_run, embed_envelope| Setup {
        embed_run,
        embed_envelope,
        ..EMBEDDING
    };
    let mut seen = Vec::new();
    for (setup, metric, said) in [
        (answered("8", CAP_REACHED), "cost_cap_reached 1", "exit 8"),
        (answered("2", NO_COST), "", "exit 2"),
        (answered("0", NO_COST), "", "states no cost"),
        (answered("7", NO_COST), "", "exit 7"),
    ] {
        let ran = shadow(setup, None)?;
        seen.push((
            ran.exit,
            ran.docker.len(),
            ran.embed_metric.contains(metric) && (metric.is_empty() == ran.embed_metric.is_empty()),
            ran.stderr.contains(said),
            ran.embed_metric.contains("last_success"),
            ran.metric
                .contains("modelgate_shadow_last_success_timestamp_seconds 1"),
        ));
    }
    assert_eq!(seen, vec![(Some(0), 3, true, true, false, true); 4]);

    let mut refused: Vec<(&str, Option<&str>)> = Vec::new();
    for (key, _) in EMBED_SETTINGS {
        refused.push((key, None));
        refused.push((key, Some("<placeholder>")));
    }
    refused.extend([
        ("MODELGATE_EMBED_TOTAL_CAP_USD", Some("3.5.0")),
        ("MODELGATE_EMBED_TOTAL_CAP_USD", Some("3.000001")),
        ("MODELGATE_EMBED_RUN_CAP_USD", Some("00.05")),
        ("MODELGATE_EMBED_PRICE_PER_MTOK", Some("1e-2")),
        ("MODELGATE_EMBED_MODEL", Some("-x")),
        ("MODELGATE_EMBED_PROVIDER", Some("a b")),
        ("MODELGATE_EMBED_PROVIDER", Some("-host")),
        ("MODELGATE_EMBED_QUANTIZATION", Some("BF16")),
        ("MODELGATE_EMBED_EPOCH", Some("01")),
        ("MODELGATE_EMBED_LIMIT", Some("0")),
        ("MODELGATE_EMBED_LIMIT", Some("2.5")),
    ]);
    let mut off = Vec::new();
    for (key, value) in &refused {
        let ran = shadow(
            Setup {
                also: Some((key, *value)),
                ..EMBEDDING
            },
            None,
        )?;
        let named = ran.stderr.contains(key) && ran.stderr.contains("the embed step is off");
        off.push((ran.exit, ran.docker.len(), named, ran.embed_metric));
    }
    assert_eq!(
        (
            off.len(),
            off.iter()
                .filter(|case| **case != (Some(0), 2, true, String::new()))
                .count()
        ),
        (29, 0),
        "{off:?}"
    );
    let placeholders = template(true)?;
    assert_eq!(
        placeholders
            .iter()
            .map(|(key, value)| (key.as_str(), value.starts_with('<')))
            .collect::<Vec<_>>(),
        EMBED_SETTINGS.map(|(key, _)| (key, true))
    );
    // 14.99 + 0.01 is the ceiling exactly and stays on.
    let at_ceiling = shadow(
        Setup {
            setting: Some(("MODELGATE_TOTAL_CAP_USD", Some("14.99"))),
            also: Some(("MODELGATE_EMBED_TOTAL_CAP_USD", Some("0.01"))),
            ..EMBEDDING
        },
        None,
    )?;
    assert_eq!((at_ceiling.exit, at_ceiling.docker.len()), (Some(0), 3));
    Ok(())
}

/// A dry run plans the embed step too, with no network, no key and no metric; a limit and an
/// acknowledgement given by hand reach the embed call alone. Mutations 2026-11-24, each alone:
/// the embed step's dry branch given the live one's network and store mount; `--embed-limit`
/// not passed on: red each.
#[test]
fn a_dry_run_plans_the_embed_step_offline_and_its_own_arguments_reach_it_alone() -> TestResult {
    let dry = shadow(
        Setup {
            args: &["--dry-run", "--from-seq", "0"],
            key_file: false,
            ..EMBEDDING
        },
        None,
    )?;
    let last = dry.docker.last().cloned().unwrap_or_default();
    let by_hand = shadow(
        Setup {
            args: &[
                "--embed-limit",
                "5",
                "--embed-ack-cost-model",
                "2026-11-23T10:16:01.000000Z",
            ],
            ..EMBEDDING
        },
        None,
    )?;
    let calls = by_hand.docker;
    assert_eq!(
        (
            dry.exit,
            dry.docker.len(),
            last.contains(" --name modelgate-embed ")
                && last.contains(" --network none --mount type=bind,src=")
                && last
                    .contains("/home/.bellwether/modelgate,dst=/modelgate,readonly newsvault:test"),
            last.contains("model-key") || last.contains("bridge"),
            last.ends_with("--non-interactive --format json --dry-run"),
            dry.stdout.matches("\"dry_run\":true").count(),
            dry.embed_metric.as_str(),
            dry.metric.as_str(),
            by_hand.exit,
            calls
                .get(1)
                .is_some_and(|run| run.ends_with("--format json --limit 21")),
            calls.get(2).is_some_and(|run| run.ends_with(
                "--limit 5 --deadline @2380 --non-interactive --format json \
                 --ack-cost-model 2026-11-23T10:16:01.000000Z"
            )),
        ),
        (
            Some(0),
            3,
            true,
            false,
            true,
            2,
            "",
            OLD_METRICS,
            Some(0),
            true,
            true
        ),
        "{}{}",
        dry.stdout,
        dry.stderr
    );
    Ok(())
}

/// The two read-only reports: a cut, then one container with no network, the store read-only and
/// no key, in place of both steps; the exit is the binary's, here 4 for a store with no
/// `embed.sqlite`. Mutations 2026-11-24, each alone: `--network bridge` in place of `none`; the
/// store mounted read-write: red each.
#[test]
fn a_dedup_report_runs_offline_over_a_read_only_store_in_place_of_both_steps() -> TestResult {
    let (uid, gid) = (id("-u")?, id("-g")?);
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (args, words) in [
        (
            &["--dedup", "distribution"][..],
            "dedup distribution --store /modelgate --format json",
        ),
        (
            &["--dedup", "sample", "--seed", "7", "--per-stratum", "2"][..],
            "dedup sample --store /modelgate --format json --seed 7 --per-stratum 2 \
             --snapshot /cut/a-snapshot",
        ),
        (
            &[
                "--dedup",
                "sample",
                "--seed",
                "7",
                "--per-stratum",
                "2",
                "--space",
                "modelgate.embed/1;model=example/embed-1;epoch=1",
                "--compare-version",
                "dedup.compare/1(window_days=7;exchanges=NYSE,OTC)",
            ][..],
            "dedup sample --store /modelgate --format json --seed 7 --per-stratum 2 \
             --snapshot /cut/a-snapshot \
             --compare-version dedup.compare/1(window_days=7;exchanges=NYSE,OTC) \
             --space modelgate.embed/1;model=example/embed-1;epoch=1",
        ),
    ] {
        let ran = shadow(
            Setup {
                args,
                key_file: false,
                ..EMBEDDING
            },
            None,
        )?;
        let home = &ran.home;
        let work = ran.cache_during.first().cloned().unwrap_or_default();
        expected.push((
            Some(4),
            2,
            Some(format!(
                "run --rm --name modelgate-dedup --pull never --user {uid}:{gid} --read-only \
                 --tmpfs /tmp:rw,size=16m --cap-drop ALL --security-opt no-new-privileges \
                 --entrypoint /usr/bin/tini \
                 --mount type=bind,src={home}/home/.cache/bellwether/{work}/cut,dst=/cut,readonly \
                 --network none \
                 --mount type=bind,src={home}/home/.bellwether/modelgate,dst=/modelgate,readonly \
                 newsvault:test -- /usr/local/bin/modelgate {words}"
            )),
            String::new(),
            true,
            Vec::<String>::new(),
        ));
        seen.push((
            ran.exit,
            ran.docker.len(),
            ran.docker.last().cloned(),
            ran.stdout,
            ran.stderr.contains("input_absent"),
            ran.cache_after,
        ));
    }
    assert_eq!(seen, expected);
    // A group's name belongs to the sample alone, and is a token of the two names' letters.
    for args in [
        &["--dedup", "distribution", "--space", "x"][..],
        &["--space", "x"][..],
        &[
            "--dedup",
            "sample",
            "--seed",
            "7",
            "--per-stratum",
            "2",
            "--space",
            "a b",
        ][..],
    ] {
        let refused = shadow(Setup { args, ..EMBEDDING }, None)?;
        assert_eq!(
            (refused.exit, refused.docker.len()),
            (Some(2), 0),
            "{args:?}"
        );
    }
    Ok(())
}

/// Section 8 row MG25, the two cases in which the step is not reached. Mutations 2026-11-24,
/// each alone: the test of the seconds left made never true; the step also run after a labelling
/// that ended 7: red each.
#[test]
fn the_embed_step_is_off_when_too_little_time_is_left_or_the_labelling_did_not_end_0_or_8(
) -> TestResult {
    // The stub clock: the cut starts at 1000 and every later reading is that many seconds on.
    let at = |cut_seconds| Setup {
        cut_seconds,
        ..EMBEDDING
    };
    let (late, in_time) = (shadow(at("1355"), None)?, shadow(at("1349"), None)?);
    assert_eq!(
        (
            late.exit,
            late.docker.len(),
            late.stderr.contains("115 s are left"),
            in_time.docker.len(),
            in_time.timeouts.last().map(String::as_str),
        ),
        (Some(0), 2, true, 3, Some("--kill-after 10 121")),
        "{}",
        late.stderr
    );
    let unreachable = shadow(
        Setup {
            run: "7",
            envelope: NO_COST,
            ..EMBEDDING
        },
        None,
    )?;
    assert_eq!((unreachable.exit, unreachable.docker.len()), (Some(7), 2));
    // The comparison's own bound past the deadline ends before the container is stopped.
    let script = std::fs::read_to_string(repo("deploy/systemd/modelgate-shadow"))?;
    let seconds = |name: &str| {
        let line = script.lines().find_map(|line| line.strip_prefix(name));
        line.and_then(|value| value.parse::<i64>().ok())
    };
    let (reserve, stop) = (seconds("embed_reserve="), seconds("embed_stop="));
    assert_eq!((reserve, stop), (Some(120), Some(30)));
    assert!(reserve.unwrap_or(0) - stop.unwrap_or(0) > COMPARE_SECONDS_PAST_DEADLINE);
    Ok(())
}
