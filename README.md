# SCC: Scalable Concurrent Containers

[Work-in-progress]
- [x] std::panic::UnwindSafe
- [x] Optimization
- [x] Stress test

SCC offers scalable concurrent containers written in the Rust language. The data structures in SCC assume to be used by a database management software running on a server, ane therefore they may not efficiently work on small systems.

## scc::HashMap

scc::HashMap is a scalable in-memory unique key-value store that is targeted at highly concurrent heavy workloads. It does not distribute data to multiple shards as most concurrent hash maps do, instead only does it have a single array of entries and corresponding metadata cell array. The metadata management strategy is similar to that of Swisstable; a metadata cell which is separated from the key-value array, is a 64-byte data structure for managing consecutive sixteen entries in the key-value array. The metadata cell also has a linked list of entry arrays for hash collision resolution. scc::HashMap automatically enlarges and shrinks the capacity of its internal array automatically, and it happens without blocking other operations and threads. In order to keep the predictable latency of each operation, it does not rehash every entry in the container at once when resizing, instead it distributes the resizing workload to future access to the data structure.

### Performance

Test environment.
- OS: SUSE Linux Enterprise Server 15 SP1
- CPU: Intel(R) Xeon(R) CPU E7-8880 v4 @ 2.20GHz x 4 (4 CPUs / 88 cores)
- RAM: 1TB
- Rust: 1.48.0

Test workload.
- Insert: each thread inserts 168M records.
- Read: each thread reads 168M records.
- Remove: each thread removes 168M records.

Test data.
- Each thread is assigned a disjoint range of integers.
- The entropy of test input is very low, however scc::HashMap artificially increases the entropy.
- The hashtable is generated using the default parameters: K = u64, V = u64, and 256 entries are pre-allocated.
- In order to minimize the cost of page fault handling, all the tests were run twice, and only the best results were taken.

Test result.
|        | 11 threads     | 22 threads     | 44 threads     | 88 threads     |
|--------|----------------|----------------|----------------|----------------|
| Insert | 248.863872330s | 246.541850542s | 281.454809275s | 471.991919119s |
| Read   | 102.500104496s | 110.250855322s | 123.870267714s | 143.606594002s |
| Remove | 127.192766540s | 141.48738765s  | 169.476767746s | 280.781299976s |


