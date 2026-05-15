//! Benchmarks comparing Leapfrog Triejoin vs Hash Join
//!
//! Run with:
//!   cargo bench
//!   cargo bench -- --output-format verbose

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// ── Leapfrog Triejoin (simplified standalone version for benchmarking) ─────────

fn leapfrog_intersect(lists: &[&[u32]]) -> Vec<u32> {
    if lists.is_empty() {
        return Vec::new();
    }
    if lists.len() == 1 {
        return lists[0].to_vec();
    }

    let mut positions: Vec<usize> = vec![0; lists.len()];
    let mut result = Vec::new();

    // Check all non-empty
    if lists.iter().any(|l| l.is_empty()) {
        return result;
    }

    let mut max_val = lists.iter().enumerate()
        .map(|(i, l)| l[positions[i]])
        .max()
        .unwrap();

    'outer: loop {
        // Seek all iterators to max_val
        for (i, list) in lists.iter().enumerate() {
            let pos = positions[i];
            // Binary search from current position
            let slice = &list[pos..];
            let new_offset = match slice.binary_search(&max_val) {
                Ok(p) | Err(p) => p,
            };
            positions[i] = pos + new_offset;

            if positions[i] >= list.len() {
                break 'outer;
            }
        }

        // Check if all at max_val
        let all_equal = lists.iter().enumerate()
            .all(|(i, l)| l[positions[i]] == max_val);

        if all_equal {
            result.push(max_val);
            // Advance all
            for (i, list) in lists.iter().enumerate() {
                positions[i] += 1;
                if positions[i] >= list.len() {
                    break 'outer;
                }
            }
            max_val = lists.iter().enumerate()
                .map(|(i, l)| l[positions[i]])
                .max()
                .unwrap();
        } else {
            max_val = lists.iter().enumerate()
                .map(|(i, l)| l[positions[i]])
                .max()
                .unwrap();
        }
    }

    result
}

// ── Hash Join (classic two-way join, repeated for N-way) ──────────────────────

fn hash_intersect(lists: &[&[u32]]) -> Vec<u32> {
    if lists.is_empty() {
        return Vec::new();
    }
    let mut result: std::collections::HashSet<u32> = lists[0].iter().copied().collect();
    for list in &lists[1..] {
        let set: std::collections::HashSet<u32> = list.iter().copied().collect();
        result = result.intersection(&set).copied().collect();
    }
    let mut v: Vec<u32> = result.into_iter().collect();
    v.sort_unstable();
    v
}

// ── Merge Join (sorted merge, two-way) ────────────────────────────────────────

fn merge_intersect_two(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    result
}

fn merge_intersect(lists: &[&[u32]]) -> Vec<u32> {
    if lists.is_empty() { return Vec::new(); }
    let mut result = lists[0].to_vec();
    for list in &lists[1..] {
        result = merge_intersect_two(&result, list);
    }
    result
}

// ── Test data generators ───────────────────────────────────────────────────────

/// Generate a sorted list of `n` values sampled from 0..domain with given density.
fn gen_sorted_list(n: usize, domain: u32, seed: u64) -> Vec<u32> {
    // Simple LCG for reproducibility
    let mut rng = seed;
    let mut set = std::collections::BTreeSet::new();
    while set.len() < n {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        set.insert((rng >> 33) as u32 % domain);
    }
    set.into_iter().collect()
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

fn bench_two_way_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("2-way join");

    for size in [1_000u32, 10_000, 100_000, 1_000_000] {
        let domain = size * 10;
        let a = gen_sorted_list(size as usize, domain, 42);
        let b = gen_sorted_list(size as usize, domain, 137);

        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("leapfrog", size), &size, |bench, _| {
            bench.iter(|| leapfrog_intersect(black_box(&[a.as_slice(), b.as_slice()])))
        });

        group.bench_with_input(BenchmarkId::new("merge", size), &size, |bench, _| {
            bench.iter(|| merge_intersect(black_box(&[a.as_slice(), b.as_slice()])))
        });

        group.bench_with_input(BenchmarkId::new("hash", size), &size, |bench, _| {
            bench.iter(|| hash_intersect(black_box(&[a.as_slice(), b.as_slice()])))
        });
    }
    group.finish();
}

fn bench_three_way_join(c: &mut Criterion) {
    let mut group = c.benchmark_group("3-way join (SPARQL BGP typical)");

    for size in [1_000u32, 10_000, 100_000] {
        let domain = size * 10;
        let a = gen_sorted_list(size as usize, domain, 42);
        let b = gen_sorted_list(size as usize, domain, 137);
        let cc = gen_sorted_list(size as usize, domain, 999);

        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(BenchmarkId::new("leapfrog", size), &size, |bench, _| {
            bench.iter(|| leapfrog_intersect(black_box(&[a.as_slice(), b.as_slice(), cc.as_slice()])))
        });

        group.bench_with_input(BenchmarkId::new("merge_sequential", size), &size, |bench, _| {
            bench.iter(|| merge_intersect(black_box(&[a.as_slice(), b.as_slice(), cc.as_slice()])))
        });

        group.bench_with_input(BenchmarkId::new("hash", size), &size, |bench, _| {
            bench.iter(|| hash_intersect(black_box(&[a.as_slice(), b.as_slice(), cc.as_slice()])))
        });
    }
    group.finish();
}

fn bench_skewed_join(c: &mut Criterion) {
    // Simulate bio RDF: one large set (all proteins), one small set (specific taxon proteins)
    // This is the realistic case where Leapfrog wins most dramatically.
    let mut group = c.benchmark_group("skewed join (bio RDF pattern)");

    let large = gen_sorted_list(1_000_000, 10_000_000, 1);  // all proteins
    let medium = gen_sorted_list(20_000, 10_000_000, 2);     // human proteins
    let small = gen_sorted_list(500, 10_000_000, 3);          // proteins with specific GO term

    group.bench_function("leapfrog (large∩medium∩small)", |bench| {
        bench.iter(|| leapfrog_intersect(black_box(&[
            large.as_slice(), medium.as_slice(), small.as_slice()
        ])))
    });

    group.bench_function("merge sequential (large∩medium∩small)", |bench| {
        bench.iter(|| merge_intersect(black_box(&[
            large.as_slice(), medium.as_slice(), small.as_slice()
        ])))
    });

    group.bench_function("hash (large∩medium∩small)", |bench| {
        bench.iter(|| hash_intersect(black_box(&[
            large.as_slice(), medium.as_slice(), small.as_slice()
        ])))
    });

    // Leapfrog advantage: with optimal ordering (small first), it skips most of large
    group.bench_function("leapfrog (small∩medium∩large — optimal order)", |bench| {
        bench.iter(|| leapfrog_intersect(black_box(&[
            small.as_slice(), medium.as_slice(), large.as_slice()
        ])))
    });

    group.finish();
}

criterion_group!(benches, bench_two_way_join, bench_three_way_join, bench_skewed_join);
criterion_main!(benches);
