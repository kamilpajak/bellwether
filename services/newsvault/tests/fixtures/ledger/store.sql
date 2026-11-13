-- A hand-written coverage fixture: the reader's output is a function of these rows and the
-- `--at` it is given, which is what makes tests/fixtures/ledger/expected.json a golden file.
-- The ids are readable rather than 64 hex characters on purpose: a golden file nobody can check
-- by eye is not a golden file, and the reader treats every id as an opaque string.
--
-- What each day holds:
--   2026-07-01  three runs of six, one unfinished; two edgar attempts over ONE window whose item
--               sets differ (result-digest drift), one opened-and-never-answered, one
--               answered-and-never-finalised, and a second source
--   2026-07-02  two runs where the schedule names one, every source closing EMPTY
--   2026-07-03  every source closing FAILED, which must not read the same as 07-02, plus
--               one attempt the next day's sweep closed as interrupted
--   2026-07-04  a vendor that answered 59 rows while the store holds none for it
--   2026-07-05  one ordinary run; its envelope is what the truncation tests overwrite
--   2026-07-30  the newest asking clock and the newest publication value, 30 min apart
--   2026-08-01  a run AFTER the `--at` every test passes, so a reader that ignores the cutoff
--               reports its clock instead of 07-30's; one of its items was PUBLISHED before the
--               cutoff and OBSERVED after it, which is the half-question the reader must refuse
-- `seq` 6 is absent: a range a transaction reserved and rolled back, so the unpublished spans
-- must break across it instead of claiming a row the store does not hold.

INSERT INTO blobs (digest, bytes, first_indexed_at) VALUES
  ('b-0701-a', 1024, '2026-07-01T00:34:30.000000Z'),
  ('b-0701-b', 2048, '2026-07-01T06:30:30.000000Z'),
  ('b-0701-p',  512, '2026-07-01T00:34:40.000000Z'),
  ('b-0705-a',  777, '2026-07-05T09:30:30.000000Z'),
  ('b-0730-a',  888, '2026-07-30T13:00:30.000000Z'),
  ('b-0801-a',  999, '2026-08-01T13:00:30.000000Z');

INSERT INTO attempts
  (attempt_id, run_id, source, target_window, filter_params, opened_at, requested_at,
   received_at, transport_status, clock_nonmonotonic, outcome, vendor_rows, items_new,
   baseline_count, baseline_window, baseline_construction, baseline_ratio, slots, closed_at,
   noticed_at, noticed_by, universe_version)
VALUES
  ('a-0701-edgar-0', 'run-0701-0034', 'edgar_press', '2026-07-01', 'fp-index',
   '2026-07-01T00:34:21.000000Z', '2026-07-01T00:34:22.000000Z', '2026-07-01T00:34:25.000000Z',
   200, 0, 'ok', 59, 2, 45, 3, 'trailing median', 1.31, 1, '2026-07-01T00:34:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0701-edgar-1', 'run-0701-0630', 'edgar_press', '2026-07-01', 'fp-index',
   '2026-07-01T06:30:21.000000Z', '2026-07-01T06:30:22.000000Z', '2026-07-01T06:30:25.000000Z',
   200, 0, 'ok', 61, 1, 45, 3, 'trailing median', 1.35, 1, '2026-07-01T06:30:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0701-polygon-0', 'run-0701-0034', 'polygon', '2026-07-01', 'fp-grouped',
   '2026-07-01T00:34:31.000000Z', '2026-07-01T00:34:32.000000Z', '2026-07-01T00:34:35.000000Z',
   200, 0, 'ok', 12, 1, 12, 1, 'trailing median', 1.0, 1, '2026-07-01T00:34:40.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0701-edgar-2', 'run-0701-1230', 'edgar_press', '2026-07-01', 'fp-index',
   '2026-07-01T12:30:21.000000Z', NULL, NULL, NULL, NULL, NULL, NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0701-edgar-3', 'run-0701-1230', 'edgar_press', '2026-07-01', 'fp-exhibit',
   '2026-07-01T12:30:41.000000Z', '2026-07-01T12:30:42.000000Z', '2026-07-01T12:30:45.000000Z',
   200, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0702-edgar-0', 'run-0702-a', 'edgar_press', '2026-07-02', 'fp-index',
   '2026-07-02T00:34:21.000000Z', '2026-07-02T00:34:22.000000Z', '2026-07-02T00:34:25.000000Z',
   200, 0, 'empty', 0, 0, 45, 3, 'trailing median', 0.0, 1, '2026-07-02T00:34:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0702-polygon-0', 'run-0702-a', 'polygon', '2026-07-02', 'fp-grouped',
   '2026-07-02T00:34:31.000000Z', '2026-07-02T00:34:32.000000Z', '2026-07-02T00:34:35.000000Z',
   200, 0, 'empty', 0, 0, 12, 1, 'trailing median', 0.0, 1, '2026-07-02T00:34:40.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0703-edgar-0', 'run-0703-a', 'edgar_press', '2026-07-03', 'fp-index',
   '2026-07-03T00:34:21.000000Z', '2026-07-03T00:34:22.000000Z', '2026-07-03T00:34:25.000000Z',
   403, 0, 'failed', 0, 0, 45, 3, 'trailing median', 0.0, 1, '2026-07-03T00:34:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0703-polygon-0', 'run-0703-a', 'polygon', '2026-07-03', 'fp-grouped',
   '2026-07-03T00:34:31.000000Z', '2026-07-03T00:34:32.000000Z', '2026-07-03T00:34:35.000000Z',
   429, 0, 'failed', 0, 0, 12, 1, 'trailing median', 0.0, 1, '2026-07-03T00:34:40.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  -- The next run's sweep found this one in flight and closed it: `abandon` writes a `Finalised`
  -- row with no counts, which is the only producer of the `interrupted` label.
  ('a-0703-edgar-1', 'run-0703-a', 'edgar_press', '2026-07-03', 'fp-exhibit',
   '2026-07-03T00:34:51.000000Z', NULL, NULL, NULL, NULL, 'interrupted', NULL, NULL,
   NULL, NULL, NULL, NULL, NULL, '2026-07-04T00:34:05.000000Z',
   '2026-07-04T00:34:05.000000Z', 'run-0704-a', 'uv-roster-2026-05-29'),
  ('a-0704-edgar-0', 'run-0704-a', 'edgar_press', '2026-07-04', 'fp-index',
   '2026-07-04T00:34:21.000000Z', '2026-07-04T00:34:22.000000Z', '2026-07-04T00:34:25.000000Z',
   200, 0, 'ok', 59, 0, 45, 3, 'trailing median', 1.31, 1, '2026-07-04T00:34:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0705-edgar-0', 'run-0705-a', 'edgar_press', '2026-07-05', 'fp-index',
   '2026-07-05T09:30:21.000000Z', '2026-07-05T09:30:22.000000Z', '2026-07-05T09:30:25.000000Z',
   200, 0, 'ok', 5, 1, 45, 3, 'trailing median', 0.11, 1, '2026-07-05T09:30:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0730-edgar-0', 'run-0730-a', 'edgar_press', '2026-07-30', 'fp-index',
   '2026-07-30T12:59:51.000000Z', '2026-07-30T13:00:00.000000Z', '2026-07-30T13:00:25.000000Z',
   200, 0, 'ok', 70, 2, 45, 3, 'trailing median', 1.55, 1, '2026-07-30T13:00:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29'),
  ('a-0801-edgar-0', 'run-0801-a', 'edgar_press', '2026-08-01', 'fp-index',
   '2026-08-01T12:59:51.000000Z', '2026-08-01T13:00:00.000000Z', '2026-08-01T13:00:25.000000Z',
   200, 0, 'ok', 90, 1, 45, 3, 'trailing median', 2.0, 1, '2026-08-01T13:00:30.000000Z',
   NULL, NULL, 'uv-roster-2026-05-29');

INSERT INTO items
  (seq, item_id, story_id, doc_id, attempt_id, valid_at, valid_refused, observed_at,
   valid_disputed, earliest_valid_at, raw_blob, body_blob, committed_at)
VALUES
  (1, 'i-0701-a', 's-0701-a', NULL, 'a-0701-edgar-0', '2026-07-01T00:10:00.000000Z', NULL,
   '2026-07-01T00:34:25.000000Z', 0, NULL, 'b-0701-a', NULL, '2026-07-01T00:34:30.000000Z'),
  (2, 'i-0701-b', 's-0701-b', NULL, 'a-0701-edgar-0', '2026-07-01T00:20:00.000000Z', NULL,
   '2026-07-01T00:34:25.000000Z', 0, NULL, 'b-0701-a', NULL, '2026-07-01T00:34:30.000000Z'),
  (3, 'i-0701-c', 's-0701-c', NULL, 'a-0701-edgar-1', '2026-07-01T06:00:00.000000Z', NULL,
   '2026-07-01T06:30:25.000000Z', 0, NULL, 'b-0701-b', NULL, '2026-07-01T06:30:30.000000Z'),
  (4, 'i-0701-p', 's-0701-p', NULL, 'a-0701-polygon-0', '2026-07-01T00:05:00.000000Z', NULL,
   '2026-07-01T00:34:35.000000Z', 0, NULL, 'b-0701-p', NULL, '2026-07-01T00:34:40.000000Z'),
  (5, 'i-0705-a', 's-0705-a', NULL, 'a-0705-edgar-0', '2026-07-05T09:00:00.000000Z', NULL,
   '2026-07-05T09:30:25.000000Z', 0, NULL, 'b-0705-a', NULL, '2026-07-05T09:30:30.000000Z'),
  (7, 'i-0730-a', 's-0730-a', NULL, 'a-0730-edgar-0', '2026-07-30T12:30:00.000000Z', NULL,
   '2026-07-30T13:00:25.000000Z', 0, NULL, 'b-0730-a', NULL, '2026-07-30T13:00:30.000000Z'),
  (8, 'i-0730-b', 's-0730-b', NULL, 'a-0730-edgar-0', NULL, '2026-07-30T18:00:00.000000Z',
   '2026-07-30T13:00:25.000000Z', 0, NULL, 'b-0730-a', NULL, '2026-07-30T13:00:30.000000Z'),
  (9, 'i-0801-a', 's-0801-a', NULL, 'a-0801-edgar-0', '2026-08-01T12:00:00.000000Z', NULL,
   '2026-08-01T13:00:25.000000Z', 0, NULL, 'b-0801-a', NULL, '2026-08-01T13:00:30.000000Z'),
  (10, 'i-0801-b', 's-0801-b', NULL, 'a-0801-edgar-0', '2026-07-30T23:00:00.000000Z', NULL,
   '2026-08-02T10:00:00.000000Z', 0, NULL, 'b-0801-a', NULL, '2026-08-02T10:00:00.000000Z');
