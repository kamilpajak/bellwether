"""Measured power on the corrected panel.

Same procedure as the 2026-11-11 run that replaced the analytic 88.2% with a measured
27.4%: inject a floor-sized effect into a transplanted treatment vector and run the
registered randomization, which matches a whole session's salience vector to a different
session. A column permutation of the treatment matrix, so the "one loud core treats its
whole partner list" dependence travels with the draw.

What changed in the INPUT, not the method:
  - the panel is built from the complete append-only lake, not the 200-item/day view that
    the earlier run read without noticing;
  - HTML tags are stripped before matching (35.9% of lake items carry them);
  - the 4,000-character truncation is gone (it discarded 50.5% of occurrences);
  - 18 cores are sieved out as generic name components, non-companies or places, with
    "Premier" restored after it was found to be a real company.
Treated company-sessions went from 4,715 to 6,527, +38.4%.

The prediction being tested: power barely moves, because the binding constraint was the
47 exchangeable sessions and not the row count. Written down before the run.

NEVER evaluates the identity assignment: every permutation is rejected if any session
would land on its own news, so the registered look stays unspent.
"""
from __future__ import annotations
import json, os, pathlib, sys

import numpy as np
import pandas as pd

# ---------------------------------------------------------------------- inputs
# Paths come from the environment, with a named abort when one is absent. This is
# requirement R6 ("a missing input aborts, never becomes an empty result") applied
# to the measurement scripts themselves, and it is why these files are committed:
# a number quoted in a design document needs the code that produced it to still run.
def _path(var: str, what: str) -> pathlib.Path:
    v = os.environ.get(var)
    if not v:
        raise SystemExit(f"brak {var} w srodowisku ({what})")
    p = pathlib.Path(v).expanduser()
    if not p.exists():
        raise SystemExit(f"{var} wskazuje na nieistniejaca sciezke: {p}")
    return p


def _out(var: str, default: str) -> pathlib.Path:
    return pathlib.Path(os.environ.get(var) or default).expanduser()


PANEL = _path("PANEL_DIR", "assign.json + meta.json from panel_recount.py")
PX = _path("PRICE_STORE", "one parquet per session, columns T and c")
FLOORS = (0.03, 0.05, 0.10)
ALPHA_LOOSE = 0.05
ALPHA_SLICE = 0.05 / 284
N_NULL, N_TRUTH, SEED = 24_000, 1_000, 20261111


def main() -> int:
    meta = json.loads((PANEL / "meta.json").read_text())
    assign = json.loads((PANEL / "assign.json").read_text())
    sessions = sorted(assign)
    uni = meta["tickers"]
    idx = {t: i for i, t in enumerate(uni)}
    n, K = len(uni), len(sessions)
    edge = np.zeros(n, bool)
    for t in meta["edge_holders"]:
        if t in idx:
            edge[idx[t]] = True
    print(f"spolek {n} | sesji {K} | z krawedzia {int(edge.sum())}", flush=True)
    print(f"konfiguracja: {meta['config']}", flush=True)

    Y = np.full((n, K), np.nan); P = np.zeros((n, K), bool); M = np.zeros((n, K), bool)
    for j, d in enumerate(sessions):
        a = assign[d]
        s = {}
        for key, day in (("0", d), ("1", a["next"])):
            df = pd.read_parquet(PX / f"{day}.parquet", columns=["T", "c"]).dropna(subset=["c"])
            df = df[~df["T"].duplicated()]
            s[key] = dict(zip(df["T"].astype(str), df["c"].astype(float)))
        for t in a["present"]:
            c0, c1 = s["0"].get(t), s["1"].get(t)
            if c0 and c1 and c0 > 0:
                Y[idx[t], j] = c1 / c0 - 1.0
                P[idx[t], j] = True
        for t in a["treated"]:
            M[idx[t], j] = True
        Y[:, j] -= np.nanmean(Y[:, j][P[:, j]])
    lo, hi = np.percentile(Y[P], [1, 99])
    Y = np.clip(Y, lo, hi)
    sd = float(np.std(Y[P], ddof=1))
    Yf = np.where(P, Y, 0.0)
    n1, n0 = int((M & P).sum()), int((edge[:, None] & ~M & P).sum())
    print(f"wierszy {int(P.sum()):,} | leczonych {n1:,} | kontrola C3 {n0:,}")
    print(f"sd(y) = {sd*1e4:.1f} pb | winsoryzacja [{lo*1e4:+.1f}; {hi*1e4:+.1f}] pb")

    A = np.zeros((K, K)); N1 = np.zeros((K, K), np.int64)
    C = np.zeros((K, K)); N0 = np.zeros((K, K), np.int64)
    OT = np.zeros((K, K, K), np.int64); OC = np.zeros((K, K, K), np.int64)
    for j in range(K):
        Pj, Yj = P[:, j], Yf[:, j]
        Ma = M & Pj[:, None]
        Ca = (edge[:, None] & ~M) & Pj[:, None]
        A[:, j] = Yj @ Ma; N1[:, j] = Ma.sum(0)
        C[:, j] = Yj @ Ca; N0[:, j] = Ca.sum(0)
        OT[:, :, j] = Ma.T.astype(np.int64) @ (M & Pj[:, None])
        OC[:, :, j] = Ca.T.astype(np.int64) @ (M & Pj[:, None])
    jj = np.arange(K)
    rng = np.random.default_rng(SEED)

    def draw():
        while True:
            p = rng.permutation(K)
            if not np.any(p == jj):
                return p

    def stat(p, p0=None, delta=0.0):
        s1, k1 = A[p, jj].sum(), N1[p, jj].sum()
        s0, k0 = C[p, jj].sum(), N0[p, jj].sum()
        if k1 == 0 or k0 == 0:
            return np.nan
        if p0 is not None and delta:
            s1 += delta * OT[p, p0, jj].sum()
            s0 += delta * OC[p, p0, jj].sum()
        return s1 / k1 - s0 / k0

    perms = np.array([draw() for _ in range(N_NULL)])
    null0 = np.array([stat(p) for p in perms]); null0 = null0[~np.isnan(null0)]
    form = np.sqrt(1 / n1 + 1 / n0) * sd
    ns = float(np.std(null0, ddof=1))
    print(f"\nrozklad zerowy: {len(null0)} losowan")
    print(f"   se ZMIERZONE        = {ns*1e4:6.2f} pb")
    print(f"   se z WZORU (iid)    = {form*1e4:6.2f} pb")
    print(f"   przeszacowanie      = {ns/form:.2f}x")
    print(f"   najmniejsze p       = {1/(len(null0)+1):.2e}  (jednostek wymiennych {K})")

    JJ = jj[None, :]
    S1 = A[perms, JJ].sum(1); KA = N1[perms, JJ].sum(1)
    S0 = C[perms, JJ].sum(1); KB = N0[perms, JJ].sum(1)
    ok = (KA > 0) & (KB > 0)
    print(f"\n{'prog':>6} {'pb':>7} {'moc@0.05':>10} {'+-MC':>6} {'moc@slice':>10} {'+-MC':>6}")
    res = {}
    for f in FLOORS:
        delta = f * sd
        rl = rs = 0
        for _ in range(N_TRUTH):
            p0 = draw()
            t0 = abs(stat(p0, p0, delta))
            ot = OT[perms, p0[None, :], JJ].sum(1)
            oc = OC[perms, p0[None, :], JJ].sum(1)
            vals = (S1 + delta * ot) / np.maximum(KA, 1) - (S0 + delta * oc) / np.maximum(KB, 1)
            nd = vals[ok]
            pv = (1 + np.sum(np.abs(nd) >= t0)) / (len(nd) + 1)
            rl += pv < ALPHA_LOOSE; rs += pv < ALPHA_SLICE
        pl, ps = rl / N_TRUTH, rs / N_TRUTH
        mc = lambda q: np.sqrt(q * (1 - q) / N_TRUTH)
        print(f"{f:6.2f} {f*sd*1e4:7.1f} {pl*100:9.1f}% {mc(pl)*100:5.1f} "
              f"{ps*100:9.1f}% {mc(ps)*100:5.1f}", flush=True)
        res[str(f)] = {"bps": f * sd * 1e4, "loose": pl, "slice": ps, "mc": float(mc(pl))}
    _out("OUT_JSON", "./panel_power.json").write_text(json.dumps({
        "treated": n1, "control_c3": n0, "rows": int(P.sum()), "sd_y_bps": sd * 1e4,
        "null_sd_bps": ns * 1e4, "formula_bps": form * 1e4, "inflation": ns / form,
        "sessions": K, "power": res, "seed": SEED,
    }, indent=1))
    print(f"\nzapisano {_out('OUT_JSON', './panel_power.json')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
