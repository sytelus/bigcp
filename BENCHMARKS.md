# Benchmarks

No performance claim has been certified yet. Initial implementation testing
was correctness-only and intentionally bounded to small C: sandboxes; no
external drive, endurance, million-entry, 20 GiB-file, or competitor sweep was
run.

Future entries must record OS build, CPU/RAM, source/destination volume and
filesystem, controller/transport, device policy, workload, warmup, repetitions,
logical and physical bytes written, average/variance, observed ceiling, command
line, commit, and thermal/AV context. Each entry must also record the
destination fragmentation evidence produced by `bigcp-testkit extents`
(files, total/max physical extents, fragmented-file count — read-only
measurement of test-owned trees): parallel copies claiming performance must
show they did not fragment large destination files, since preallocation is
the designed counter-measure (DESIGN.md) and a regression there would
otherwise be invisible until HDD read-back slowed. All benchmark workloads are bounded
(low-GB budgets); endurance/TB-class measurement is prohibited outright by
VISION — there is no approval path — so million-file and TB-class figures, when
stated, are extrapolations from bounded results plus design analysis and must
be labeled as such.
