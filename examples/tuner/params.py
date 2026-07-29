# Tunable knobs of the serving pipeline. The agent edits THESE to minimize p99 latency.
# Ranges: BATCH_SIZE 1..64, WORKERS 1..16, PREFETCH 0..8.
BATCH_SIZE = 8
WORKERS = 2
PREFETCH = 1
