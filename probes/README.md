# probes

A probe is a measurement script kept so its number can be re-derived, not a service and not
production code. It answers one question about the outside world, writes a result file, and is
expected to be run rarely.

Two rules, both of which exist because of the predecessor's history:

**A probe's decision rule is written before it runs.** The memo's doctrine is to pre-register before
testing, and that applies to a vendor evaluation as much as to a study: a threshold chosen after
seeing the number is not a threshold. Each probe therefore ships a `PREREGISTRATION.md` stating
what each possible answer will mean, committed before any key exists.

**A probe is not a vendor client.** It talks to a vendor URL directly, which production code here
may not do — every vendor call in a service goes through that vendor's one canonical client. If a
vendor is adopted, its calls move behind such a client and the probe stays as the record of why.
