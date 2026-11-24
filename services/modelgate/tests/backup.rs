//! The gate's store in the daily backup (architecture.md section 3.5, "The store's daily copy"):
//! `modelgate snapshot` and `modelgate verify` on a store a stubbed run wrote, and
//! `deploy/systemd/newsvault-backup modelgate` against a stub `docker`, `flock` and `timeout`
//! whose `docker run` execs the real binary. No test here opens a socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use modelgate::cmd_shadow::{self, Args, Env};
use modelgate::ledger::SendPermit;
use modelgate::store::{Store, BLOB_DIR, DB_FILE};
use modelgate::wire::{Exchanged, HttpAnswer, ModelExchange};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The shell the unit runs the script with: Ubuntu's `/bin/sh`.
const UNIT_SHELL: &str = "/bin/dash";
/// Dated before the fixture's observations of 2026-07-01 to 2026-07-03.
const MODEL: &str = "example/decide-1-20260101";
const NOW: &str = "2026-07-03T12:00:00Z";
/// Bytes of the key file and of a file planted in the store; no archive member may hold them.
const SECRET: &str = "not-a-real-key-7f3a";
/// A blob file no row names, as a run killed between a blob and its row leaves one.
const UNNAMED_BLOB: &str = "00000000000000000000000000000000000000000000000000000000000000aa";
const STAGING: &str = ".staging-killed";
const PROM: &str = "modelgate-backup.prom";
const VAULT_PROM: &str = "newsvault-backup.prom";
const OLD_METRICS: &str = "modelgate_backup_last_success_bytes 1\n";
const FOLDER: &str = "ncdata/ncuser/files/Backups/bellwether";
const DAYS: [&str; 8] = ["01", "02", "03", "04", "05", "06", "07", "08"];
/// Stands in for docker. `run`: records its words on one line, then execs the real binary on
/// the words after `/usr/local/bin/modelgate`, `/modelgate` and `/out` mapped to their mounts.
/// `STUB_DAMAGE` removes one blob of the copy before `verify` reads it.
/// `exec`: the Nextcloud container, `/mnt/ncdata` mapped to `$STUB_DIR/ncdata`; `occ` exits 0.
const DOCKER_STUB: &str = r#"#!/bin/sh
if [ "$1" = run ]; then
    echo "$*" >>"$STUB_DIR/docker-runs"
    for arg; do
        case $arg in
            type=bind,src=*,dst=/modelgate,readonly) store=${arg#type=bind,src=}; store=${store%%,dst=*} ;;
            type=bind,src=*,dst=/out) out=${arg#type=bind,src=}; out=${out%%,dst=*} ;;
        esac
    done
    while [ "$1" != /usr/local/bin/modelgate ]; do shift; done
    shift
    n=$#
    for arg; do
        case $arg in
            /modelgate) arg=$store ;;
            /out/*) arg="$out/${arg#/out/}" ;;
        esac
        set -- "$@" "$arg"
    done
    shift "$n"
    if [ "$1:$STUB_DAMAGE" = verify:yes ]; then
        for blob in "$out"/store/blobs/*; do rm -f "$blob"; break; done
    fi
    exec "$STUB_MODELGATE" "$@"
fi
[ "$1" = exec ] || exit 0
echo "$*" >>"$STUB_DIR/execs"
shift
while :; do
    case $1 in -i) shift ;; -u) shift 2 ;; *) break ;; esac
done
shift
n=$#
for arg; do
    case $arg in /mnt/ncdata/*) arg="$STUB_DIR/ncdata/${arg#/mnt/ncdata/}" ;; esac
    set -- "$@" "$arg"
done
shift "$n"
case $1 in
    php) exit 0 ;;
    sh | mkdir | rm | mv | ls) cmd=$1; shift; exec "/bin/$cmd" "$@" ;;
esac
exit 127
"#;
/// Stands in for flock(1) in front of a command: records its four option words and the lock
/// file; exits the conflict code when `STUB_HELD` says a shadow run holds the lock.
const FLOCK_STUB: &str = r#"#!/bin/sh
echo "$1 $2 $3 $4 $5" >>"$STUB_DIR/flock-args"
[ "$STUB_HELD" = yes ] && exit "$4"
shift 5
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

/// Answers every send with a 200 whose body the store keeps as a response blob.
struct Answering;

impl ModelExchange for Answering {
    fn send(&self, permit: SendPermit, body: &[u8]) -> Exchanged {
        let at: DateTime<Utc> = NOW.parse().unwrap_or_default();
        let answer = HttpAnswer {
            status: 200,
            body: format!("{{\"echo_bytes\": {}}}", body.len()).into_bytes(),
            retry_after_seconds: None,
        };
        Exchanged::new(permit, at, at, Ok(answer))
    }
}

/// A store one live run over the committed fixture wrote, then what a killed run and a careless
/// hand leave in its directory.
fn written_store(root: &Path) -> TestResult {
    let args = Args {
        snapshot: repo("snapshot-contract/fixtures/snapshot_v1"),
        questions: repo("services/modelgate/questions/ripple-shadow-1.json"),
        model: MODEL.to_owned(),
        run_cap_usd: "0.40".to_owned(),
        day_cap_usd: "1.50".to_owned(),
        total_cap_usd: "40".to_owned(),
        input_price_per_mtok: "0.042".to_owned(),
        give_up_after_hours: 24,
        store: root.to_path_buf(),
        from_seq: Some(0),
        limit: None,
        sample_only: false,
        repeat_index: 0,
        ack_cost_model: None,
        dry_run: false,
    };
    let env = Env {
        exchange: &Answering,
        now: &|| NOW.parse().unwrap_or_default(),
        sleep: &|_: Duration| {},
    };
    cmd_shadow::run(&args, &env).map_err(|failure| failure.to_string())?;
    std::fs::write(root.join(BLOB_DIR).join(UNNAMED_BLOB), "no row names this")?;
    std::fs::write(root.join(BLOB_DIR).join(STAGING), "half a blob")?;
    std::fs::write(root.join("model-key"), SECRET)?;
    Ok(())
}

fn count(root: &Path, sql: &str) -> TestResult<i64> {
    Ok(Connection::open(root.join(DB_FILE))?.query_row(sql, [], |row| row.get(0))?)
}

fn modelgate(args: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_modelgate"))
        .args(args)
        .output()?)
}

fn json(bytes: &[u8]) -> TestResult<Value> {
    Ok(serde_json::from_slice(bytes)?)
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

fn copy_name(store: &str, day: &str) -> String {
    format!("bellwether-{store}-200001{day}T233000Z.tar.gz")
}

struct Backed {
    work: TempDir,
    exit: Option<i32>,
    stderr: String,
    flock: Vec<String>,
    runs: Vec<String>,
    execs: Vec<String>,
    folder: Vec<String>,
}

impl Backed {
    fn path(&self, relative: &str) -> PathBuf {
        self.work.path().join(relative)
    }
}

/// One run of `newsvault-backup modelgate` over [`written_store`], the folder already holding
/// eight copies of each store.
fn backup(held: bool, timeout: &str, damage: bool) -> TestResult<Backed> {
    let work = TempDir::new()?;
    let at = |relative: &str| work.path().join(relative);
    for dir in [
        "home/.config/bellwether",
        "stub/bin",
        "run",
        "textfile",
        &format!("stub/{FOLDER}"),
    ] {
        std::fs::create_dir_all(at(dir))?;
    }
    written_store(&at("home/.bellwether/modelgate"))?;
    std::fs::write(at("home/.config/bellwether/model-key"), SECRET)?;
    for prom in [PROM, VAULT_PROM] {
        std::fs::write(at("textfile").join(prom), OLD_METRICS)?;
    }
    for day in DAYS {
        for store in ["vault", "modelgate"] {
            std::fs::write(
                at(&format!("stub/{FOLDER}")).join(copy_name(store, day)),
                "",
            )?;
        }
    }
    for (name, text) in [
        ("docker", DOCKER_STUB),
        ("flock", FLOCK_STUB),
        ("timeout", TIMEOUT_STUB),
    ] {
        executable(&at("stub/bin").join(name), text)?;
    }
    let output = Command::new(UNIT_SHELL)
        .arg(repo("deploy/systemd/newsvault-backup"))
        .arg("modelgate")
        .current_dir(work.path())
        .env_clear()
        .env("HOME", at("home"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", at("stub/bin").display()),
        )
        .env("XDG_RUNTIME_DIR", at("run"))
        .env("NEWSVAULT_IMAGE", "newsvault:test")
        .env("NEWSVAULT_TEXTFILE_DIR", at("textfile"))
        .env("NEXTCLOUD_CONTAINER", "nextcloud-aio-nextcloud")
        .env("NEXTCLOUD_DATA", "/mnt/ncdata")
        .env("NEXTCLOUD_USER", "ncuser")
        .env("NEXTCLOUD_FOLDER", "Backups/bellwether")
        .env("STUB_DIR", at("stub"))
        .env("STUB_MODELGATE", env!("CARGO_BIN_EXE_modelgate"))
        .env("STUB_HELD", if held { "yes" } else { "no" })
        .env("STUB_TIMEOUT", timeout)
        .env("STUB_DAMAGE", if damage { "yes" } else { "no" })
        .stdin(Stdio::null())
        .output()?;
    Ok(Backed {
        exit: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        flock: lines(&at("stub/flock-args")),
        runs: lines(&at("stub/docker-runs")),
        execs: lines(&at("stub/execs")),
        folder: names(&at(&format!("stub/{FOLDER}"))),
        work,
    })
}

/// Section 8 row MG19. The archive is `gate.sqlite` and the blobs its rows name: not the unnamed
/// blob, the staging file, `LOCK` or a key file. Unpacked into an empty directory, `verify` reads
/// the source's row counts and blob count. The snapshot runs under the shadow lock with a bounded
/// wait, the store read-only and no network; retention deletes the gate's oldest copy and none of
/// the vault's.
#[test]
fn the_archive_is_the_database_and_its_named_blobs_and_a_restore_verifies_with_the_sources_counts(
) -> TestResult {
    let backed = backup(false, "", false)?;
    assert_eq!(backed.exit, Some(0), "{}", backed.stderr);
    let source = backed.path("home/.bellwether/modelgate");
    let new: Vec<&String> = backed
        .folder
        .iter()
        .filter(|name| name.starts_with("bellwether-modelgate-2") && !name.contains("-2000"))
        .collect();
    let [archive] = new.as_slice() else {
        return Err(format!("{:?}", backed.folder).into());
    };
    let restored = backed.path("restored");
    std::fs::create_dir(&restored)?;
    let unpacked = Command::new("tar")
        .arg("-xzf")
        .arg(backed.path(&format!("stub/{FOLDER}")).join(archive))
        .arg("-C")
        .arg(&restored)
        .status()?;
    assert!(unpacked.success());

    let verified = modelgate(&[
        "verify",
        "--store",
        &restored.to_string_lossy(),
        "--format",
        "json",
    ])?;
    assert_eq!(verified.status.code(), Some(0), "{:?}", verified.stderr);
    let envelope = json(&verified.stdout)?;
    let labels = count(&source, "SELECT COUNT(*) FROM labels")?;
    let responses = count(
        &source,
        "SELECT COUNT(DISTINCT response_sha256) FROM attempt_events",
    )?;
    let source_blobs = names(&source.join(BLOB_DIR));
    let named: Vec<String> = source_blobs
        .iter()
        .filter(|name| ![UNNAMED_BLOB, STAGING].contains(&name.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        (
            envelope["schema"].as_str(),
            envelope["rows"]["labels"].as_i64(),
            envelope["rows"]["requests"].as_i64(),
            envelope["blobs"]["named"].as_u64(),
            envelope["blobs"]["unnamed_files"].as_u64(),
            names(&restored),
            names(&restored.join(BLOB_DIR)),
        ),
        (
            Some("modelgate.store_verify/1"),
            Some(labels),
            Some(count(&source, "SELECT COUNT(*) FROM requests")?),
            Some(u64::try_from(named.len())?),
            Some(0),
            vec![BLOB_DIR.to_owned(), DB_FILE.to_owned()],
            named.clone(),
        )
    );
    // The fixture's two stories with a body, each answered: one question file, two requests,
    // at least one response.
    assert!(
        labels == 2 && responses >= 1 && named.len() >= 4,
        "{named:?}"
    );
    for name in &named {
        let bytes = std::fs::read(restored.join(BLOB_DIR).join(name))?;
        assert!(!String::from_utf8_lossy(&bytes).contains(SECRET), "{name}");
    }

    let lock = backed.path("run/bellwether-shadow.lock");
    let flags = "--pull never --user";
    let hardening = "--read-only --network none --cap-drop ALL --security-opt no-new-privileges \
                     --entrypoint /usr/bin/tini";
    let snapshot_run = backed.runs.first().cloned().unwrap_or_default();
    assert_eq!(
        (
            backed.flock.clone(),
            backed.runs.len(),
            snapshot_run.starts_with(&format!("run --rm --name modelgate-backup {flags} ")),
            snapshot_run.contains(hardening),
            snapshot_run.contains(&format!(
                "type=bind,src={},dst=/modelgate,readonly",
                source.display()
            )),
            snapshot_run.ends_with(
                "newsvault:test -- /usr/local/bin/modelgate snapshot --store /modelgate \
                 --out /out/store --non-interactive --format json"
            ),
            backed.runs.get(1).is_some_and(|run| {
                run.starts_with(&format!("run --rm --name modelgate-backup-verify {flags} "))
                    && run.contains(hardening)
                    && !run.contains("dst=/modelgate")
                    && run.ends_with(
                        "-- /usr/local/bin/modelgate verify --store /out/store \
                         --non-interactive --format json",
                    )
            }),
        ),
        (
            vec![format!(
                "--timeout 760 --conflict-exit-code 7 {}",
                lock.display()
            )],
            2,
            true,
            true,
            true,
            true,
            true
        )
    );

    let mut kept: Vec<String> = DAYS.iter().map(|day| copy_name("vault", day)).collect();
    kept.extend(DAYS.iter().skip(2).map(|day| copy_name("modelgate", day)));
    kept.push((*archive).clone());
    kept.sort();
    let metric = std::fs::read_to_string(backed.path("textfile").join(PROM))?;
    let families: Vec<&str> = metric
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split(' ').next())
        .collect();
    assert_eq!(
        (
            backed.folder.clone(),
            families,
            std::fs::read_to_string(backed.path("textfile").join(VAULT_PROM))?,
            names(&backed.path("home/.cache/bellwether")),
        ),
        (
            kept,
            vec![
                "modelgate_backup_last_success_timestamp_seconds",
                "modelgate_backup_last_success_bytes"
            ],
            OLD_METRICS.to_owned(),
            Vec::<String>::new(),
        )
    );
    Ok(())
}

/// A shadow run that keeps its lock past the wait, a snapshot past its bound, and a copy that
/// lost a blob before `verify` read it: the exit and its reason, nothing written to Nextcloud,
/// nothing deleted, the last metric kept, the staging directory gone.
#[test]
fn a_held_lock_a_snapshot_past_its_bound_or_a_copy_verify_refuses_writes_nothing() -> TestResult {
    let mut seen = Vec::new();
    let cases = [
        (true, "", false, 7, "a shadow run held its lock for 760 s"),
        (
            false,
            "124",
            false,
            7,
            "the snapshot passed its 150 s bound",
        ),
        (
            false,
            "137",
            false,
            7,
            "the snapshot passed its 150 s bound",
        ),
        (false, "", true, 2, "io_failed"),
    ];
    for (held, timeout, damage, exit, reason) in cases {
        let backed = backup(held, timeout, damage)?;
        seen.push((
            backed.exit == Some(exit) && backed.stderr.contains(reason),
            backed.runs.len(),
            backed.execs.len(),
            backed.folder.len(),
            std::fs::read_to_string(backed.path("textfile").join(PROM))?,
            names(&backed.path("home/.cache/bellwether")),
        ));
    }
    let refused = |runs: usize| {
        (
            true,
            runs,
            0,
            2 * DAYS.len(),
            OLD_METRICS.to_owned(),
            Vec::<String>::new(),
        )
    };
    assert_eq!(seen, [0, 0, 0, 2].map(refused).to_vec());
    Ok(())
}

fn code(output: &Output) -> TestResult<(Option<i32>, String, usize)> {
    let object = json(&output.stderr)?;
    Ok((
        output.status.code(),
        object["code"].as_str().unwrap_or_default().to_owned(),
        output.stdout.len(),
    ))
}

/// The snapshot waits for no writer and writes into nothing that holds a file; a copy that lost
/// or changed one named blob is refused by `verify`, which names it.
#[test]
fn a_snapshot_is_refused_under_a_writers_lease_and_verify_names_a_missing_or_altered_blob(
) -> TestResult {
    let work = TempDir::new()?;
    let at = |relative: &str| work.path().join(relative);
    let text = |relative: &str| at(relative).to_string_lossy().into_owned();
    written_store(&at("store"))?;
    let snapshot = |out: &str| {
        modelgate(&[
            "snapshot",
            "--store",
            &text("store"),
            "--out",
            &text(out),
            "--json",
        ])
    };
    let verify = |store: &str| modelgate(&["verify", "--store", &text(store), "--format", "json"]);

    let writer = Store::open(&at("store"))?;
    let held = code(&snapshot("held")?)?;
    drop(writer);
    std::fs::create_dir(at("full"))?;
    std::fs::write(at("full/a-file"), "")?;
    let full = code(&snapshot("full")?)?;
    std::fs::create_dir(at("never-run"))?;
    let absent = [
        code(&modelgate(&[
            "snapshot",
            "--store",
            &text("never-run"),
            "--out",
            &text("none"),
            "--json",
        ])?)?,
        code(&verify("never-run")?)?,
    ];

    let taken = snapshot("copy")?;
    assert_eq!(taken.status.code(), Some(0), "{:?}", taken.stderr);
    let envelope = json(&taken.stdout)?;
    let whole = verify("copy")?.status.code();
    let blobs = names(&at("copy").join(BLOB_DIR));
    let victim = at("copy").join(BLOB_DIR).join(&blobs[0]);
    let kept = std::fs::read(&victim)?;
    std::fs::write(&victim, "edited")?;
    let altered = verify("copy")?;
    std::fs::remove_file(&victim)?;
    let missing = verify("copy")?;
    std::fs::write(&victim, kept)?;
    assert_eq!(
        (
            held,
            full,
            absent,
            envelope["schema"].as_str(),
            envelope["blobs"]["unnamed_files"].as_u64(),
            whole,
            code(&altered)?,
            String::from_utf8_lossy(&altered.stderr).contains(&blobs[0]),
            code(&missing)?,
            String::from_utf8_lossy(&missing.stderr).contains(&blobs[0]),
            verify("copy")?.status.code(),
        ),
        (
            (Some(7), "lease_held".to_owned(), 0),
            (Some(2), "invalid_argument".to_owned(), 0),
            [
                (Some(4), "input_absent".to_owned(), 0),
                (Some(4), "input_absent".to_owned(), 0)
            ],
            Some("modelgate.snapshot/1"),
            Some(2),
            Some(0),
            (Some(2), "io_failed".to_owned(), 0),
            true,
            (Some(2), "io_failed".to_owned(), 0),
            true,
            Some(0),
        )
    );
    Ok(())
}
