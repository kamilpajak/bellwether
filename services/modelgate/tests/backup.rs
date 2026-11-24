//! The gate's store in the daily backup (architecture.md section 3.5, "The store's daily copy"):
//! `modelgate snapshot` and `modelgate verify` on a store a stubbed run wrote, and
//! `deploy/systemd/newsvault-backup modelgate` against a stub `docker`, `flock` and `timeout`
//! and `tar` whose `docker run` execs the real binary. No test here opens a socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use modelgate::cmd_shadow::{self, Args, Env};
use modelgate::ledger::SendPermit;
use modelgate::store::{Store, BLOB_DIR, DB_FILE, TABLES};
use modelgate::wire::{Exchanged, HttpAnswer, ModelExchange};
use rusqlite::Connection;
use serde_json::{json, Value};
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
/// `rm`: recorded. `exec`: the Nextcloud container, `/mnt/ncdata` mapped to `$STUB_DIR/ncdata`; `occ` exits 0.
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
if [ "$1" = rm ]; then echo "$*" >>"$STUB_DIR/docker-rms"; exit 0; fi
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
/// Stands in for timeout(1): records its three option words; exits `STUB_TIMEOUT` without
/// running the command when it is set.
const TIMEOUT_STUB: &str = r#"#!/bin/sh
echo "$1 $2 $3" >>"$STUB_DIR/timeout-args"
[ -z "$STUB_TIMEOUT" ] || exit "$STUB_TIMEOUT"
shift 3
exec "$@"
"#;
/// tar(1), or under `STUB_TAR` one that leaves part of an archive and fails.
const TAR_STUB: &str = r#"#!/bin/sh
if [ "$STUB_TAR" = fail ]; then echo part >"$2"; exit 1; fi
exec /usr/bin/tar "$@"
"#;

/// A child spawned by one test holds, until its exec, a copy of every descriptor open in this
/// process, a store's locked `LOCK` among them: with the tests in parallel `Store::open` met
/// `LeaseHeld` in 1 of 11 runs of this binary (2026-11-24). Every test here takes this first.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn alone() -> TestResult<MutexGuard<'static, ()>> {
    Ok(ONE_AT_A_TIME.lock().map_err(|_| "a test here failed")?)
}

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

fn id(flag: &str) -> TestResult<String> {
    let printed = Command::new("id").arg(flag).output()?.stdout;
    Ok(String::from_utf8(printed)?.trim().to_owned())
}

/// A recorded `docker run` with the staging directory's `mktemp` suffix replaced by its template.
fn staging_named(run: &str) -> String {
    const MARK: &str = "/backup.";
    const SUFFIX: usize = 6;
    match run.split_once(MARK) {
        Some((before, after)) if after.len() >= SUFFIX => {
            format!("{before}{MARK}XXXXXX{}", &after[SUFFIX..])
        }
        _ => run.to_owned(),
    }
}

/// Overwrites the second half of a table's root page, as a damaged disk block would.
fn damage_table(database: &Path, table: &str) -> TestResult {
    use std::io::{Seek, SeekFrom, Write};
    let db = Connection::open(database)?;
    let page: i64 = db.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    let root: i64 = db.query_row(
        "SELECT rootpage FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    drop(db);
    let (page, root) = (u64::try_from(page)?, u64::try_from(root)?);
    let half = page / 2;
    let mut file = std::fs::OpenOptions::new().write(true).open(database)?;
    file.seek(SeekFrom::Start((root - 1) * page + half))?;
    file.write_all(&vec![0x5a; usize::try_from(half)?])?;
    Ok(())
}

fn copy_name(store: &str, day: &str) -> String {
    format!("bellwether-{store}-200001{day}T233000Z.tar.gz")
}

/// What one run of the script meets.
#[derive(Clone, Copy, PartialEq)]
enum Meets {
    Nothing,
    /// A shadow run keeps its lock past the wait.
    HeldLock,
    /// `timeout` ends the snapshot with this status.
    Bound(&'static str),
    /// The copy loses a blob before `verify` reads it.
    DamagedCopy,
    FailingTar,
    NoStore,
    NoRuntimeDir,
    /// The store lacks a blob one of its rows names.
    LostSourceBlob,
}

struct Backed {
    work: TempDir,
    exit: Option<i32>,
    stderr: String,
    flock: Vec<String>,
    timeouts: Vec<String>,
    removals: Vec<String>,
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
fn backup(meets: Meets) -> TestResult<Backed> {
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
    if meets != Meets::NoStore {
        written_store(&at("home/.bellwether/modelgate"))?;
    }
    if meets == Meets::LostSourceBlob {
        let blobs = at("home/.bellwether/modelgate").join(BLOB_DIR);
        let named = names(&blobs)
            .into_iter()
            .find(|name| ![UNNAMED_BLOB, STAGING].contains(&name.as_str()));
        std::fs::remove_file(blobs.join(named.ok_or("no named blob")?))?;
    }
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
        ("tar", TAR_STUB),
    ] {
        executable(&at("stub/bin").join(name), text)?;
    }
    let mut script = Command::new(UNIT_SHELL);
    script.env_clear();
    if meets != Meets::NoRuntimeDir {
        script.env("XDG_RUNTIME_DIR", at("run"));
    }
    let bound = match meets {
        Meets::Bound(status) => status,
        _ => "",
    };
    let said = |yes: bool, word: &'static str| if yes { word } else { "no" };
    let output = script
        .arg(repo("deploy/systemd/newsvault-backup"))
        .arg("modelgate")
        .current_dir(work.path())
        .env("HOME", at("home"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", at("stub/bin").display()),
        )
        .env("NEWSVAULT_IMAGE", "newsvault:test")
        .env("NEWSVAULT_TEXTFILE_DIR", at("textfile"))
        .env("NEXTCLOUD_CONTAINER", "nextcloud-aio-nextcloud")
        .env("NEXTCLOUD_DATA", "/mnt/ncdata")
        .env("NEXTCLOUD_USER", "ncuser")
        .env("NEXTCLOUD_FOLDER", "Backups/bellwether")
        .env("STUB_DIR", at("stub"))
        .env("STUB_MODELGATE", env!("CARGO_BIN_EXE_modelgate"))
        .env("STUB_HELD", said(meets == Meets::HeldLock, "yes"))
        .env("STUB_TIMEOUT", bound)
        .env("STUB_DAMAGE", said(meets == Meets::DamagedCopy, "yes"))
        .env("STUB_TAR", said(meets == Meets::FailingTar, "fail"))
        .stdin(Stdio::null())
        .output()?;
    Ok(Backed {
        exit: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        flock: lines(&at("stub/flock-args")),
        timeouts: lines(&at("stub/timeout-args")),
        removals: lines(&at("stub/docker-rms")),
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
    let _alone = alone()?;
    let backed = backup(Meets::Nothing)?;
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
    let mut rows = serde_json::Map::new();
    let mut named_bytes = 0;
    for table in TABLES {
        let counted = count(&source, &format!("SELECT COUNT(*) FROM {table}"))?;
        rows.insert(table.to_owned(), json!(counted));
    }
    for name in &named {
        named_bytes += std::fs::metadata(source.join(BLOB_DIR).join(name))?.len();
    }
    assert_eq!(
        (
            envelope["schema"].as_str(),
            &envelope["rows"],
            envelope["blobs"]["named"].as_u64(),
            envelope["blobs"]["bytes"].as_u64(),
            envelope["blobs"]["unnamed_files"].as_u64(),
            names(&restored),
            names(&restored.join(BLOB_DIR)),
        ),
        (
            Some("modelgate.store_verify/1"),
            &Value::Object(rows),
            Some(u64::try_from(named.len())?),
            Some(named_bytes),
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
    let user = format!("{}:{}", id("-u")?, id("-g")?);
    let container = |name: &str, mounts: &str, command: &str| {
        format!(
            "run --rm --name {name} --pull never --user {user} --read-only --network none \
             --cap-drop ALL --security-opt no-new-privileges --entrypoint /usr/bin/tini \
             --mount type=bind,src={staging},dst=/out{mounts} newsvault:test -- \
             /usr/local/bin/modelgate {command} --non-interactive --format json",
            staging = backed
                .path("home/.cache/bellwether/backup.XXXXXX")
                .display()
        )
    };
    assert_eq!(
        (
            backed.flock.clone(),
            backed.timeouts.clone(),
            backed.removals.len(),
            backed.runs.iter().map(|run| staging_named(run)).collect(),
        ),
        (
            vec![format!(
                "--timeout 760 --conflict-exit-code 7 {}",
                lock.display()
            )],
            vec!["--kill-after 20 150".to_owned()],
            0,
            vec![
                container(
                    "modelgate-backup",
                    &format!(
                        " --mount type=bind,src={},dst=/modelgate,readonly",
                        source.display()
                    ),
                    "snapshot --store /modelgate --out /out/store"
                ),
                container("modelgate-backup-verify", "", "verify --store /out/store"),
            ],
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

/// A shadow run that keeps its lock past the wait, a snapshot past its bound, a copy that lost a
/// blob before `verify` read it, a `tar` that failed, no store, no runtime directory and a store
/// lacking a named blob: the exit and its reason, nothing written to Nextcloud, nothing deleted,
/// the last metric kept, the staging directory gone. Only a snapshot past its bound is removed.
#[test]
fn a_held_lock_a_snapshot_past_its_bound_or_a_copy_verify_refuses_writes_nothing() -> TestResult {
    let _alone = alone()?;
    let bound = "the snapshot passed its 150 s bound";
    let cases = [
        (
            Meets::HeldLock,
            7,
            "a shadow run held its lock for 760 s",
            0,
        ),
        (Meets::Bound("124"), 7, bound, 0),
        (Meets::Bound("137"), 7, bound, 0),
        (Meets::DamagedCopy, 2, "io_failed", 2),
        (Meets::FailingTar, 2, "", 2),
        (Meets::NoStore, 4, "no store at", 0),
        (Meets::NoRuntimeDir, 2, "XDG_RUNTIME_DIR", 0),
        (Meets::LostSourceBlob, 2, "io_failed", 1),
    ];
    let mut seen = Vec::new();
    let mut expected = Vec::new();
    for (meets, exit, reason, runs) in cases {
        let backed = backup(meets)?;
        seen.push((
            backed.exit,
            backed.stderr.contains(reason),
            backed.runs.len(),
            backed.removals.clone(),
            backed.execs.len(),
            backed.folder.len(),
            std::fs::read_to_string(backed.path("textfile").join(PROM))?,
            names(&backed.path("home/.cache/bellwether")),
        ));
        let removals = match meets {
            Meets::Bound(_) => vec!["rm --force modelgate-backup".to_owned()],
            _ => Vec::new(),
        };
        expected.push((
            Some(exit),
            true,
            runs,
            removals,
            0,
            2 * DAYS.len(),
            OLD_METRICS.to_owned(),
            Vec::<String>::new(),
        ));
    }
    assert_eq!(seen, expected);
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
    let _alone = alone()?;
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
    let from = |store: &str| {
        modelgate(&[
            "snapshot",
            "--store",
            &text(store),
            "--out",
            &text("none"),
            "--json",
        ])
    };
    std::fs::create_dir(at("lock-only"))?;
    std::fs::write(at("lock-only/LOCK"), "")?;
    let mut absent = vec![
        code(&from("never-run")?)?,
        code(&verify("never-run")?)?,
        code(&from("lock-only")?)?,
    ];

    let taken = snapshot("copy")?;
    assert_eq!(taken.status.code(), Some(0), "{:?}", taken.stderr);
    let envelope = json(&taken.stdout)?;
    // A restored store before its first live run: a database and no `LOCK`.
    absent.push(code(&from("copy")?)?);
    let whole = verify("copy")?.status.code();
    let blobs = names(&at("copy").join(BLOB_DIR));
    let victim = at("copy").join(BLOB_DIR).join(&blobs[0]);
    let kept = std::fs::read(&victim)?;
    std::fs::write(&victim, "edited")?;
    let altered = verify("copy")?;
    std::fs::remove_file(&victim)?;
    let missing = verify("copy")?;
    std::fs::write(&victim, kept)?;
    let restored = verify("copy")?.status.code();
    std::fs::remove_file(at("store").join(BLOB_DIR).join(&blobs[0]))?;
    let lost = snapshot("lost")?;
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
            restored,
        ),
        (
            (Some(7), "lease_held".to_owned(), 0),
            (Some(2), "invalid_argument".to_owned(), 0),
            vec![(Some(4), "input_absent".to_owned(), 0); 4],
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
    assert_eq!(
        (
            code(&lost)?,
            String::from_utf8_lossy(&lost.stderr).contains(&blobs[0])
        ),
        ((Some(2), "io_failed".to_owned(), 0), true)
    );
    Ok(())
}

/// The backup mounts the store read-only: a store nothing here may write is copied. A table page
/// damaged after that is refused by `verify` in the copy and by `snapshot` in the source, where
/// `COUNT(*)` alone reads an index and the same counts.
#[test]
fn a_store_without_write_access_is_copied_and_a_damaged_table_page_is_refused() -> TestResult {
    let _alone = alone()?;
    use std::os::unix::fs::PermissionsExt;
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
    let modes = |dir: u32, file: u32| -> TestResult {
        for (path, mode) in [
            ("store/LOCK", file),
            ("store/gate.sqlite", file),
            ("store/blobs", dir),
            ("store", dir),
        ] {
            std::fs::set_permissions(at(path), std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    };
    modes(0o555, 0o444)?;
    let read_only = snapshot("copy");
    modes(0o755, 0o644)?;
    let read_only = read_only?;
    assert_eq!(read_only.status.code(), Some(0), "{:?}", read_only.stderr);

    let verify = || modelgate(&["verify", "--store", &text("copy"), "--json"]);
    let whole = verify()?.status.code();
    damage_table(&at("copy").join(DB_FILE), "labels")?;
    let copy = verify()?;
    damage_table(&at("store").join(DB_FILE), "labels")?;
    let source = snapshot("second")?;
    let names_it = |output: &Output| {
        String::from_utf8_lossy(&output.stderr).contains("is not a whole database")
    };
    assert_eq!(
        (
            whole,
            code(&copy)?,
            names_it(&copy),
            code(&source)?,
            names_it(&source)
        ),
        (
            Some(0),
            (Some(2), "io_failed".to_owned(), 0),
            true,
            (Some(2), "io_failed".to_owned(), 0),
            true
        )
    );
    Ok(())
}
