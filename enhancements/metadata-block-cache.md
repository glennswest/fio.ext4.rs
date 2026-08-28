# Metadata block cache — 280x write amplification measured

**Filed:** 2026-08-28, from sbregistry's per-layer device stats on rose1.

## Measurement

Unpacking one tar layer of 9,730,568 bytes (a single 9.7 MB file) into an
ext4 volume over NVMe/TCP produced:

    684,093 writes (2,702 MiB), 684,337 reads (2,665 MiB),
    1,368,436 operations, 112.2 s awaiting the device, 0.08 ms per op

~280x write amplification, and a read paired with nearly every write. The
device is not the problem — 0.08 ms per op, ~12,000 ops/s sustained. The op
COUNT is the problem: ~288 device operations per 4 KiB of payload.

The pattern implicates per-append metadata churn: block bitmap, inode
table, extent tree, group descriptor and superblock counters re-read and
re-written from the device for every few KiB of file data, with the
BlockDevice acting as the only cache. Transport-side fixes were tried first
and acquitted the lower layers: queue-depth window, overlap-aware read
ordering, 1 MiB write coalescing, boundary-only RMW — layer time barely
moved (165 s → ~90–128 s), because interleaved far-offset metadata ops
keep contiguous runs from ever forming.

## Ask

Cache hot metadata blocks in the Volume/writer layer — even a small LRU
(bitmaps, inode table blocks, extent nodes, group descriptors), flushed at
sync points (file end, flush(), unmount) — or batch metadata commits per
file. Expected effect on the measured layer: ~1.37 M ops → a few thousand;
~128 s → well under a second. Downstream this takes a 55 MB image's golden
build on rose1 from ~30 minutes to seconds, which is the difference the
whole clone-based boot design assumes.

Consistency note: the consumer (sbregistry) treats a build that dies
mid-stream as discard-and-rebuild, and calls flush() before seal — lazy
metadata durability between sync points is acceptable there by design.
