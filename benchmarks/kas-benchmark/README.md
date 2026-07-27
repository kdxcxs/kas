# KAS end-to-end benchmark

This crate benchmarks KAS as a black box. It starts real `kas-api`,
`kas-builtin-driver`, and generated singleton Driver processes, installs
generated packages through HTTP, creates Resources through HTTP, reconciles
them over the production WebSocket protocol, runs a mixed steady workload, and
writes machine-readable reports. It never reads or writes SQLite directly.

Run the short functional benchmark:

```bash
./benchmarks/kas-benchmark/run.sh smoke
```

Run every one-dimensional scale point:

```bash
./benchmarks/kas-benchmark/run.sh sweep \
  --profile benchmarks/kas-benchmark/profiles/scale.json
```

Find the first Resource count that violates the configured SLO:

```bash
./benchmarks/kas-benchmark/run.sh find-limit \
  --profile benchmarks/kas-benchmark/profiles/limit.json \
  --dimension resources \
  --start 1000 \
  --max 1000000
```

Limit points are repeated three times by default and decided by majority. This
reduces false boundaries caused by scheduler and filesystem noise before the
exponential search switches to binary refinement.

Results are written below `benchmark-results/`. Each scenario has its exact
configuration, request samples, process samples, logs, JSON summary, and
Markdown report. Temporary database contents are removed after a successful
run unless `keep_data` is enabled.

The supported sweep/limit dimensions are:

- `resources`
- `manifests`
- `drivers`
- `resource_bytes`
- `spec_fields`
- `spec_depth`
- `watch_fanout`
- `write_concurrency`
- `read_concurrency`
- `reconcile_delay_ms`

`drivers` cannot exceed `manifests`, and `watch_fanout` cannot exceed
`drivers`. Resource size is the serialized create request size; the summary
records the actual generated average.
