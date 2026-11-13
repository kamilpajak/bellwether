"""The observation lag of EDGAR's daily index, which is requirement R11's one missing input.

Measures, per sampled filing: (the instant the vendor states it last wrote form.<day>.idx)
minus (that filing's <ACCEPTANCE-DATETIME>), in whole seconds, which is what
pit::MeasuredLag.seconds holds. Every band, every tolerance and the request budget were
registered before the first request: PREREGISTRATION.md beside this file. Four things here are
NOT in the registered text and are in that file's amendment dated 2026-11-13, written before the
first request and after a review of the code: the scalar is the MAX and not the p90, a write
instant outside the day its file names is a refusal, a day with no row of our form has its own
code, and one of the two vendor statements is enough.

Stdlib only, no vendor client: a probe talks to a vendor URL directly, which production code
here may not do. Aborts with a named code when an input is absent (requirement R6).
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.request
from email.utils import parsedate_to_datetime
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

OUT = pathlib.Path(__file__).parent / "observation_lag_results.json"
PROBE_ID_PREFIX = "edgar_index_write"

ARCHIVES = "https://www.sec.gov/Archives"
DAILY_INDEX = f"{ARCHIVES}/edgar/daily-index"
FORM_TYPE = "8-K"
ZONE = "America/New_York"
ACCEPTANCE_FIELD = "<ACCEPTANCE-DATETIME>"
ACCEPTANCE_FORMAT = "%Y%m%d%H%M%S"
LAST_DATA_RECEIVED = "Last Data Received:"
LAST_MODIFIED = "Last-Modified"
LASTMOD_NOTE = f"{LAST_MODIFIED} response header"
LISTED_WRITE_FIELD = "last-modified"
# The two shapes EDGAR's directory JSON is documented and observed to use. A value present in
# this field and matching neither aborts rather than reading as absent.
LISTED_WRITE_FORMATS = ("%m/%d/%Y %I:%M:%S %p", "%Y-%m-%d %H:%M:%S")

SAMPLE_PER_DAY = 12
DEFAULT_BUDGET = 30
# A fifth of SEC's stated 10 req/s per-IP ceiling (predecessor's vendor-quota table, read
# 2026-11-13, unmeasured here). The bucket is per IP and this host also runs the Rust
# collector and the predecessor's production EDGAR path.
MIN_INTERVAL_S = 0.5
# edgar-press's VENDOR_PATIENCE, which is an assumption there and stays one here.
TIMEOUT_S = 30.0
# <ACCEPTANCE-DATETIME> sits on the third line of a filing's header.
DOC_PREFIX_BYTES = 16 * 1024
# Half edgar-press's BODY_LIMIT, against an index of about 600 KB.
INDEX_READ_CAP = 4 * 1024 * 1024

P90 = 0.9
TOLERANCE_S = 60
HOUR_S = 3600
DAY_S = 24 * HOUR_S
WEEK_S = 7 * DAY_S
# A write instant this far past the end of the day the file names is a rewrite and not that
# day's close, so the construction is measuring the latest rewrite: refused, not registered.
REWRITE_ALLOWANCE_S = DAY_S

OK = 200
PARTIAL = 206
NO_ANSWER = 0
EXIT_REFUSED = 2


class Aborted(Exception):
    """A named code and a detail. An empty result in place of one is forbidden (R6). Its exit is
    2 and never 1, because 1 in this project means "the answer is no"."""

    def __init__(self, code: str, detail: str) -> None:
        super().__init__(f"{code}: {detail}")
        self.code = code
        self.detail = detail


def contact_from_env() -> str:
    value = os.environ.get("SEC_CONTACT", "").strip()
    if not value:
        raise Aborted(
            "contact_absent",
            "SEC_CONTACT is unset. SEC answers a User-Agent with no contact address with 403.\n"
            "  SEC_CONTACT='Jane Doe jane@example.com'",
        )
    # The same check as edgar-press's usable_contact: one token holding local@domain.tld.
    addressed = any(
        "@" in token and token.split("@", 1)[0] and "." in token.split("@", 1)[1]
        for token in value.split()
    )
    if not addressed:
        raise Aborted("contact_unaddressed", "SEC_CONTACT carries no e-mail address")
    return value


def zone() -> ZoneInfo:
    try:
        return ZoneInfo(ZONE)
    except ZoneInfoNotFoundError as why:
        raise Aborted("zone_absent", f"no tz database entry for {ZONE}: {why}") from why


def days_from(given: list[str]) -> list[dt.date]:
    try:
        return [dt.date.fromisoformat(day) for day in given]
    except ValueError as why:
        raise Aborted("days_unreadable", f"--days wants YYYY-MM-DD: {why}") from why


class Vendor:
    """Every request to the vendor, counted and paced. A request that failed is still counted:
    "nothing came back" and "we did not look" must stay different."""

    def __init__(self, contact: str, budget: int) -> None:
        self._headers = {"User-Agent": contact, "Accept-Encoding": "identity"}
        self.budget = budget
        self.spent = 0
        self._next_allowed = 0.0

    def get(self, url: str, cap: int, byte_range: str | None = None) -> tuple[int, dict, bytes]:
        if self.spent + 1 > self.budget:
            raise Aborted(
                "budget_exhausted",
                f"{self.spent} requests spent of a budget of {self.budget}; "
                f"the next would be {url}",
            )
        wait = self._next_allowed - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        headers = dict(self._headers)
        if byte_range is not None:
            headers["Range"] = byte_range
        request = urllib.request.Request(url, headers=headers)
        self.spent += 1
        self._next_allowed = time.monotonic() + MIN_INTERVAL_S
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT_S) as answer:
                return answer.status, dict(answer.headers), answer.read(cap + 1)
        except urllib.error.HTTPError as answer:
            return answer.status, dict(answer.headers), answer.read(cap + 1)
        except (urllib.error.URLError, TimeoutError) as why:
            return NO_ANSWER, {}, type(why).__name__.encode()


def budget_from(given: int) -> int:
    """The registration's ceiling is a ceiling and not a default. Without this, `--budget 500`
    is a registered run by the document's letter."""
    if given > DEFAULT_BUDGET:
        raise Aborted(
            "budget_above_registration",
            f"--budget {given} is over the registered ceiling of {DEFAULT_BUDGET}; "
            "no run under this registration may exceed it",
        )
    return given


def quarter_of(day: dt.date) -> int:
    return (day.month - 1) // 3 + 1


def listing_url(day: dt.date) -> str:
    return f"{DAILY_INDEX}/{day.year}/QTR{quarter_of(day)}/index.json"


def index_name(day: dt.date) -> str:
    return f"form.{day:%Y%m%d}.idx"


def index_url(day: dt.date) -> str:
    return f"{DAILY_INDEX}/{day.year}/QTR{quarter_of(day)}/{index_name(day)}"


def require_ok(url: str, status: int, payload: bytes) -> None:
    if status != OK:
        raise Aborted("http_failed", f"{url} answered {status}: {payload[:120]!r}")


def header_write_instant(headers: dict) -> dt.datetime | None:
    raw = headers.get(LAST_MODIFIED)
    if not raw:
        return None
    try:
        return parsedate_to_datetime(raw).astimezone(dt.timezone.utc)
    except (TypeError, ValueError) as why:
        raise Aborted(
            "write_instant_unreadable", f"{LAST_MODIFIED}: {raw!r} is not an HTTP date: {why}"
        ) from why


def listed_entries(payload: bytes) -> dict[str, dict]:
    """The quarter listing's own entry per file. It carries two things no other read in this tree
    has: the second path to the write instant, and the vendor's own stated byte size."""
    try:
        read = json.loads(payload)
    except ValueError as why:
        raise Aborted("listing_unreadable", f"the quarter listing is not JSON: {why}") from why
    items = read.get("directory", {}).get("item")
    if not isinstance(items, list):
        raise Aborted("listing_unreadable", "the quarter listing holds no directory.item array")
    return {item["name"]: item for item in items if isinstance(item, dict) and item.get("name")}


def listed_write_instants(entries: dict[str, dict], tz: ZoneInfo) -> dict[str, dt.datetime]:
    """The committed fixture carries no such field, so whether EDGAR serves it is part of the
    answer. One statement is enough and the report names which one it used."""
    out: dict[str, dt.datetime] = {}
    for item in entries.values():
        name, raw = item.get("name"), item.get(LISTED_WRITE_FIELD)
        if not name or not raw:
            continue
        parsed = None
        for shape in LISTED_WRITE_FORMATS:
            try:
                parsed = dt.datetime.strptime(raw, shape)
            except ValueError:
                continue
            break
        if parsed is None:
            raise Aborted(
                "listing_instant_unreadable",
                f"{name}'s {LISTED_WRITE_FIELD} is {raw!r}, which matches none of "
                f"{LISTED_WRITE_FORMATS}; reading it as absent would be an empty fallback",
            )
        out[name] = parsed.replace(tzinfo=tz).astimezone(dt.timezone.utc)
    return out


def index_rows(text: str) -> list[dict]:
    """The same rule edgar-press's `index_rows` applies, so the probe samples the rows production
    would see: rows after the dash separator, first token the form type and the last three the
    CIK, the date filed and the path."""
    lines = text.splitlines()
    separator = next(
        (i for i, line in enumerate(lines) if line.strip() and set(line.strip()) == {"-"}),
        None,
    )
    if separator is None:
        raise Aborted("index_unreadable", "the daily index holds no dash separator line")
    rows = []
    for line in lines[separator + 1 :]:
        fields = line.split()
        if len(fields) < 4:
            continue
        rows.append({"form": fields[0], "cik": fields[-3], "filed": fields[-2], "path": fields[-1]})
    return rows


def last_data_received(text: str) -> str | None:
    for line in text.splitlines():
        if line.strip().startswith(LAST_DATA_RECEIVED):
            return line.split(":", 1)[1].strip()
    return None


def sampled(rows: list[dict], k: int) -> list[dict]:
    """Evenly spaced over the day's rows in file order, decided before any instant is read: the
    file is ordered by form type and then company name, so the first k would be one end of it."""
    n = len(rows)
    if n == 0:
        return []
    return [rows[min(i * n // k, n - 1)] for i in range(min(k, n))]


def acceptance_of(prefix: str, tz: ZoneInfo) -> dt.datetime | None:
    for line in prefix.splitlines():
        stripped = line.strip()
        if not stripped.startswith(ACCEPTANCE_FIELD):
            continue
        raw = stripped[len(ACCEPTANCE_FIELD) :].strip()
        try:
            naive = dt.datetime.strptime(raw, ACCEPTANCE_FORMAT)
        except ValueError:
            return None
        return naive.replace(tzinfo=tz).astimezone(dt.timezone.utc)
    return None


def quantile(values: list[int], fraction: float) -> int | None:
    """The definition probes/vendor_probe/probe.py uses: the value at index
    min(floor(fraction x n), n - 1) of the ascending list."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(int(fraction * len(ordered)), len(ordered) - 1)]


def collect_day(
    vendor: Vendor,
    day: dt.date,
    listed: dict[str, dt.datetime],
    entries: dict[str, dict],
    tz: ZoneInfo,
) -> dict:
    url = index_url(day)
    status, headers, payload = vendor.get(url, INDEX_READ_CAP)
    require_ok(url, status, payload)
    if len(payload) > INDEX_READ_CAP:
        raise Aborted("index_truncated", f"{url} is larger than the {INDEX_READ_CAP} B read cap")
    text = payload.decode("utf-8", "replace")
    rows = index_rows(text)
    of_form = [row for row in rows if row["form"] == FORM_TYPE]
    named = f"{day:%Y%m%d}"
    filings = []
    for row in sampled(of_form, SAMPLE_PER_DAY):
        doc_url = f"{ARCHIVES}/{row['path']}"
        doc_status, _, doc_payload = vendor.get(
            doc_url, DOC_PREFIX_BYTES, byte_range=f"bytes=0-{DOC_PREFIX_BYTES - 1}"
        )
        accession = pathlib.PurePosixPath(row["path"]).stem
        # The index's own Date Filed for this row. The real committed fixture holds 3 rows of 502
        # filed up to 8 days before the index that carries them, so this is the shape that makes a
        # per-filing lag days rather than hours, and it is counted rather than filtered.
        off_day = row["filed"] != named
        if doc_status not in (OK, PARTIAL):
            filings.append(
                {
                    "accession": accession,
                    "filed": row["filed"],
                    "filed_off_day": off_day,
                    "unusable": f"the document answered {doc_status}",
                }
            )
            continue
        accepted = acceptance_of(doc_payload.decode("utf-8", "replace"), tz)
        if accepted is None:
            filings.append(
                {
                    "accession": accession,
                    "filed": row["filed"],
                    "filed_off_day": off_day,
                    "unusable": f"no {ACCEPTANCE_FIELD} in the first {DOC_PREFIX_BYTES} B",
                }
            )
            continue
        filings.append(
            {
                "accession": accession,
                "cik": row["cik"],
                "filed": row["filed"],
                "filed_off_day": off_day,
                "accepted_at": accepted.isoformat(),
            }
        )
    listed_instant = listed.get(index_name(day))
    header_instant = header_write_instant(headers)
    entry = entries.get(index_name(day), {})
    return {
        "day": day.isoformat(),
        "rows_in_index": len(rows),
        "rows_of_form": len(of_form),
        "rows_of_form_off_day": sum(1 for row in of_form if row["filed"] != named),
        "bytes_downloaded": len(payload),
        "bytes_vendor_stated": entry.get("size"),
        "write_instant_header": header_instant.isoformat() if header_instant else None,
        "write_instant_listing": listed_instant.isoformat() if listed_instant else None,
        "last_data_received": last_data_received(text),
        "filings": filings,
    }


def inside_named_day(named: dt.date, written: dt.datetime, tz: ZoneInfo) -> None:
    """The one condition that can falsify the construction from a single read, and the datum the
    registration's own 'what this probe cannot settle' says would void it. A daily index whose
    last write is not the close of the day it names is a file that was rewritten, and then every
    lag taken from it is that rewrite's distance and not a publication lag."""
    opens = dt.datetime.combine(named, dt.time.min, tzinfo=tz).astimezone(dt.timezone.utc)
    closes = opens + dt.timedelta(seconds=DAY_S + REWRITE_ALLOWANCE_S)
    if opens <= written <= closes:
        return
    raise Aborted(
        "write_instant_outside_named_day",
        f"{named}: the vendor's stated last write {written.isoformat()} is outside the day the "
        f"file names, {opens.isoformat()} to {closes.isoformat()} (the day in {ZONE} plus a "
        f"stated {REWRITE_ALLOWANCE_S} s allowance); the instant is a rewrite's and not the "
        "day's close, so no lag taken from it is a publication lag",
    )


def write_instant(day: dict, tz: ZoneInfo) -> tuple[dt.datetime, str]:
    """The header's instant, cross-read against the quarter listing's. Both absent, or the two
    further apart than the registered tolerance, and nothing may be registered. ONE statement is
    enough and the second value returned names which one was used, because the expected case is
    one being served: the committed listing fixture carries the field on no entry."""
    header, listed = day["write_instant_header"], day["write_instant_listing"]
    if header is None and listed is None:
        raise Aborted(
            "write_instant_absent",
            f"{day['day']}: the vendor states no last-write instant for its daily index, "
            "neither by response header nor in the quarter listing",
        )
    if header is not None and listed is not None:
        apart = abs(
            (dt.datetime.fromisoformat(header) - dt.datetime.fromisoformat(listed)).total_seconds()
        )
        if apart > TOLERANCE_S:
            raise Aborted(
                "write_instants_disagree",
                f"{day['day']}: {LAST_MODIFIED} {header} and the listing's {listed} are "
                f"{int(apart)} s apart, over the registered {TOLERANCE_S} s tolerance",
            )
        statement = f"both, {int(apart)} s apart"
    elif header is not None:
        statement = f"{LAST_MODIFIED} alone, the listing serving no {LISTED_WRITE_FIELD}"
    else:
        statement = f"the quarter listing alone, no {LAST_MODIFIED} served"
    written = dt.datetime.fromisoformat(header or listed)
    inside_named_day(dt.date.fromisoformat(day["day"]), written, tz)
    return written, statement


def lags_of(days: list[dict], tz: ZoneInfo) -> tuple[list[int], list[dict]]:
    """Both instants are second-granular, so whole seconds truncate nothing."""
    lags, at_or_before = [], []
    for day in days:
        written, statement = write_instant(day, tz)
        day["write_instant_used"] = statement
        day["lags"] = []
        for filing in day["filings"]:
            if "accepted_at" not in filing:
                continue
            accepted = dt.datetime.fromisoformat(filing["accepted_at"])
            seconds = int((written - accepted).total_seconds())
            filing["lag_seconds"] = seconds
            lags.append(seconds)
            day["lags"].append(seconds)
            if seconds <= 0:
                at_or_before.append({"accession": filing["accession"], "lag_seconds": seconds})
    return lags, at_or_before


def band_of(p90: int) -> tuple[str, str]:
    if p90 <= DAY_S:
        return "<= 24 h", "backfill permitted; the leak rate travels with the lag and is disclosed"
    if p90 <= WEEK_S:
        return (
            "24 h - 7 d",
            "backfill permitted only with the session shift written into the panel; "
            "no day column may mix backfilled and live items",
        )
    return "> 7 d", "this construction is closed; the polling probe is the only remaining path"


def per_day(days: list[dict]) -> list[dict]:
    """The exchangeable unit for a write instant is the DAY, so every quantity is reported per day
    as well as pooled. The pooled scalar is a MAX, and a max over days is the max of the per-day
    maxima, so pooling cannot dilute it the way a pooled quantile can."""
    return [
        {
            "day": day["day"],
            "probes": len(day["lags"]),
            "write_instant": day.get("write_instant_used"),
            "median_seconds": quantile(day["lags"], 0.5),
            "p90_seconds": quantile(day["lags"], P90),
            "max_seconds": max(day["lags"]) if day["lags"] else None,
            "rows_of_form": day["rows_of_form"],
            "rows_of_form_off_day": day["rows_of_form_off_day"],
        }
        for day in days
    ]


def decide(days: list[dict], tz: ZoneInfo) -> dict:
    """Every branch here is a row of PREREGISTRATION.md's outcome tables, plus the two the
    amendment dated 2026-11-13 in that same file adds: the registered scalar is the MAX of the
    per-filing lags and not their p90, and a day whose index carries no row of our form is its own
    refusal and not a thin sample."""
    requested = sum(len(day["filings"]) for day in days)
    empty = [day["day"] for day in days if day["rows_of_form"] == 0]
    if empty:
        return {
            "registered": None,
            "refused_because": "no_rows_of_form",
            "detail": f"the index carried no {FORM_TYPE} row at all on {', '.join(empty)}; "
            "the vendor's index holding nothing of our form and the documents not answering are "
            "two different facts and do not share a code",
            "probes": 0,
            "requested": requested,
        }
    lags, at_or_before = lags_of(days, tz)
    usable = len(lags)
    if usable * 2 < requested:
        return {
            "registered": None,
            "refused_because": "sample_too_thin",
            "detail": f"{usable} of {requested} sampled documents usable, which is fewer than "
            "half; a distribution over whichever documents answered is a silent partial",
            "probes": usable,
            "requested": requested,
            "per_day": per_day(days),
        }
    if at_or_before:
        return {
            "registered": None,
            "refused_because": "negative_or_zero_lag",
            "detail": f"{len(at_or_before)} of {usable} sampled filings carry a lag of 0 s or "
            "less; a file cannot have been last written before a filing it contains was accepted, "
            "so the instant is not that file's write instant for at least those rows",
            "filings": at_or_before,
            "probes": usable,
            "requested": requested,
            "per_day": per_day(days),
        }
    # The scalar dates every backfilled item through observed = valid + seconds, so it has to
    # cover the WORST sampled lag or the rows above it are dated before the file that first
    # carried them existed. The registration's own asymmetry argument says this and then names
    # the p90; the p90 is reported beside it and decides nothing.
    seconds = max(lags)
    p90 = quantile(lags, P90)
    band, meaning = band_of(seconds)
    return {
        "registered": {
            "probe_id": f"{PROBE_ID_PREFIX}-{dt.datetime.now(dt.timezone.utc):%Y%m%d}",
            "probes": usable,
            "leaks": sum(1 for lag in lags if lag > seconds),
            "seconds": seconds,
        },
        "band": band,
        "meaning": meaning,
        "median_seconds": quantile(lags, 0.5),
        "p90_seconds": p90,
        "p90_band": band_of(p90)[0],
        "p90_would_have_left_above_it": sum(1 for lag in lags if lag > p90),
        "max_seconds": seconds,
        "probes": usable,
        "requested": requested,
        "per_day": per_day(days),
    }


def report(days: list[dict], verdict: dict, spent: int, budget: int) -> None:
    print(
        "construction: lag = (the instant the vendor states it last wrote form.<day>.idx) minus"
        f"\n  ({ACCEPTANCE_FIELD} of the filing, {ZONE} -> UTC), in whole seconds"
    )
    for day in days:
        usable = [filing for filing in day["filings"] if "lag_seconds" in filing]
        print(
            f"\n{day['day']}: {day['rows_of_form']} {FORM_TYPE} rows of {day['rows_in_index']} "
            f"in the index, {day['rows_of_form_off_day']} of them carrying a Date Filed that is "
            "NOT this index's own day"
            f"\n  {day['bytes_downloaded']} B downloaded against {day['bytes_vendor_stated']!r} "
            "stated by the vendor's own quarter listing, which rounds to KB and is not bytes"
            f"\n  write instant: header {day['write_instant_header']}, quarter listing "
            f"{day['write_instant_listing']}  [used: {day.get('write_instant_used')}; "
            f"{TOLERANCE_S} s tolerance when both are served]"
            f"\n  the index's own 'Last Data Received' is {day['last_data_received']!r}: a DATE, "
            "never folded into an instant, printed to show which day the file claims"
            f"\n  sampled {len(day['filings'])} of {day['rows_of_form']} rows [row "
            f"floor(i x n / {SAMPLE_PER_DAY}) in file order], {len(usable)} usable"
            "\n  each lag below is that one write instant minus the filing's own acceptance, so "
            "the spread across the rows is the spread of acceptance times and not of writes"
        )
        for filing in usable:
            off = "  OFF-DAY, filed " + filing["filed"] if filing["filed_off_day"] else ""
            print(
                f"    {filing['accession']}: {filing['lag_seconds']:>8} s = "
                f"{filing['lag_seconds'] / HOUR_S:6.2f} h  [accepted {filing['accepted_at']}]{off}"
            )
        for filing in (filing for filing in day["filings"] if "unusable" in filing):
            print(f"    {filing['accession']}: UNUSABLE, {filing['unusable']}")
    print(
        f"\nrequests spent: {spent} of a budget of {budget}  [1 quarter listing, then 1 daily "
        f"index and up to {SAMPLE_PER_DAY} documents per day, paced at one per {MIN_INTERVAL_S} s]"
    )
    if verdict["registered"] is None:
        print(
            f"\nNOTHING REGISTERED: {verdict['refused_because']}"
            f"\n  {verdict['detail']}"
            f"\n  over {verdict['probes']} usable of {verdict['requested']} sampled filings; "
            "the rule was decided before the run, in PREREGISTRATION.md's outcome tables"
        )
        return
    lag = verdict["registered"]
    print("\nper day, because the exchangeable unit for a write instant is the DAY:")
    for day in verdict["per_day"]:
        print(
            f"  {day['day']}: n={day['probes']}, median {day['median_seconds']} s, p90 "
            f"{day['p90_seconds']} s, max {day['max_seconds']} s, write instant {day['write_instant']}"
        )
    print(
        f"\nregistered lag: {lag['seconds']} s = {lag['seconds'] / HOUR_S:.2f} h"
        f"\n  [the MAX of {lag['probes']} per-filing lags over {len(days)} days; median "
        f"{verdict['median_seconds']} s, p90 {verdict['p90_seconds']} s]"
        f"\n  band {verdict['band']}: {verdict['meaning']}"
        f"\n  leaks {lag['leaks']} of {lag['probes']} probes. A leak is a sampled filing whose own "
        "lag exceeds the registered scalar; under a max there can be none, so this field is 0 BY "
        "CONSTRUCTION and is not a measurement of the vendor"
        f"\n  what the registered p90 rule would have given: {verdict['p90_seconds']} s, band "
        f"{verdict['p90_band']}, with {verdict['p90_would_have_left_above_it']} of {lag['probes']} "
        "sampled filings above it, each of which a backfill dated by it would claim early"
        f"\n  the vendor-movable quantity beside the lag is the off-day row count printed per day "
        "above: those rows are what makes a max days rather than hours"
    )
    print(f"\npit::MeasuredLag to pass to EdgarPress::with_observation_lag:\n  {json.dumps(lag)}")


def write_result(collected: list[dict], verdict: dict, vendor: Vendor) -> None:
    """Written whether the run finished or aborted, so a refused run is still a record of what it
    cost: the project's own rule that an attempt row exists whether or not anything came back."""
    OUT.write_text(
        json.dumps(
            {
                "fetched_at": dt.datetime.now(dt.timezone.utc).isoformat(),
                # The numbers the verdict turns on, so a reader of this file alone can re-derive
                # it. Everything else about the construction has one home, the line below.
                "construction": {
                    "registration": "probes/observation_lag/PREREGISTRATION.md",
                    "publication": f"{ACCEPTANCE_FIELD} parsed {ACCEPTANCE_FORMAT} in {ZONE}",
                    "observation": f"the vendor's stated last write of form.<day>.idx, the "
                    f"{LASTMOD_NOTE} cross-read against the quarter listing",
                    "form_types": [FORM_TYPE],
                    "sample_rule": f"row floor(i x n / {SAMPLE_PER_DAY}) in index file order",
                    "scalar_definition": "the MAX of the per-filing lags (amendment 2026-11-13)",
                    "p90_definition": f"index min(floor({P90} x n), n - 1) ascending",
                    "tolerance_seconds": TOLERANCE_S,
                    "rewrite_allowance_seconds": REWRITE_ALLOWANCE_S,
                    "pace_seconds": MIN_INTERVAL_S,
                    "requests_spent": vendor.spent,
                    "budget": vendor.budget,
                },
                "days": collected,
                "verdict": verdict,
            },
            indent=1,
        )
    )
    print(f"\nwritten {OUT} after {vendor.spent} requests")


def run(args: argparse.Namespace) -> int:
    days = days_from(args.days)
    tz = zone()
    vendor = Vendor(contact_from_env(), budget_from(args.budget))
    collected: list[dict] = []
    verdict: dict = {
        "registered": None,
        "refused_because": "did_not_finish",
        "detail": "the run neither registered nor refused; see stderr",
    }
    try:
        entries: dict[str, dict] = {}
        for quarter in sorted({(day.year, quarter_of(day)) for day in days}):
            first = next(day for day in days if (day.year, quarter_of(day)) == quarter)
            url = listing_url(first)
            status, _, payload = vendor.get(url, INDEX_READ_CAP)
            require_ok(url, status, payload)
            # The vendor payload as received. `collect` wants the quarter listing as a hashed
            # FILE it is handed (architecture.md section 10), and this is the one request in the
            # tree that fetches it, so the bytes are kept rather than re-fetched.
            listing_copy = OUT.parent / f"quarter_index.{quarter[0]}Q{quarter[1]}.json"
            listing_copy.write_bytes(payload)
            entries.update(listed_entries(payload))
        listed = listed_write_instants(entries, tz)
        collected = [collect_day(vendor, day, listed, entries, tz) for day in days]
        verdict = decide(collected, tz)
        report(collected, verdict, vendor.spent, vendor.budget)
        # 1 is "the answer is no": the probe ran and registered nothing. Every abort exits 2.
        return 0 if verdict["registered"] is not None else 1
    except Aborted as why:
        verdict = {
            "registered": None,
            "refused_because": why.code,
            "detail": why.detail,
        }
        raise
    finally:
        write_result(collected, verdict, vendor)


def main() -> int:
    ap = argparse.ArgumentParser(description="EDGAR daily-index observation lag (R11's input)")
    ap.add_argument("--days", nargs="+", required=True, help="UTC days, YYYY-MM-DD")
    ap.add_argument(
        "--budget",
        type=int,
        default=DEFAULT_BUDGET,
        help=f"request ceiling, lowering only: a value over {DEFAULT_BUDGET} is refused",
    )
    try:
        return run(ap.parse_args())
    except Aborted as why:
        print(why, file=sys.stderr)
        return EXIT_REFUSED


if __name__ == "__main__":
    sys.exit(main())
