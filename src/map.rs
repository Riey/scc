extern crate crossbeam_epoch;

pub mod array;
pub mod cell;
pub mod link;

use array::{Array, MAX_ENLARGE_FACTOR};
use cell::{CellLocker, CellReader};
use crossbeam_epoch::{Atomic, Owned, Shared};
use link::EntryArrayLink;
use std::convert::TryInto;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::{Acquire, Release};

/// A scalable concurrent hash map implementation.
///
/// scc::HashMap is a concurrent hash map data structure that is targeted at a highly concurrent workload.
/// The epoch-based reclamation technique provided by the crossbeam_epoch crate allows the data structure to eliminate coarse locking,
/// instead, only a small, fixed number of key-value pairs share a single mutex.
/// Therefore, it internally has only a single entry array storing key-value pairs and corresponding metadata array.
/// The metadata array is composed of metadata cells, and each of them manages a fixed number of key-value pair entries using a customized mutex.
/// The metadata cells are able to locate the correct entry by having an array of partial hash values of the key-value pairs.
/// A metadata cell resolves hash collisions by allocating a linked list of key-value pair arrays.
///
/// The key features of scc::HashMap.
/// * No sharding: all keys stored in a single entry array are managed by a single array of metadata cells.
/// * Auto resizing: it automatically enlarges or shrinks the internal arrays.
/// * Non-blocking resizing: resizing does not block other threads.
/// * Incremental resizing: each access to the data structure relocates a certain number of key-value pairs.
/// * Optimized resizing: key-value pairs in a single metadata cell are guaranteed to be relocated to adjacent cells.
/// * Minimized shared data: no atomic counter and coarse lock.
///
/// The key statistics for scc::HashMap.
/// * The expected size of metadata for a single key-value pair: 4-byte.
/// * The expected number of atomic operations required for a single key operation: 2.
/// * The expected number of atomic variables accessed during a single key operation: 1.
/// * The range of hash values a single metadata cell manages: 65536.
/// * The number of entries managed by a single metadata cell without a linked list: 16.
/// * The number of entries a single linked list entry manages: 4.
/// * The expected maximum linked list length when resize is triggered: log(capacity) / 4.
pub struct HashMap<K: Eq + Hash + Sync, V: Sync, H: BuildHasher> {
    array: Atomic<Array<K, V>>,
    minimum_capacity: usize,
    resize_mutex: AtomicBool,
    hasher: H,
}

impl<K: Eq + Hash + Sync, V: Sync, H: BuildHasher> HashMap<K, V, H> {
    /// Creates an empty HashMap instance with the given hasher and minimum capacity.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), Some(1000));
    ///
    /// let result = hashmap.capacity();
    /// assert_eq!(result, 1024);
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    /// let result = hashmap.capacity();
    /// assert_eq!(result, 256);
    /// ```
    pub fn new(hasher: H, minimum_capacity: Option<usize>) -> HashMap<K, V, H> {
        let initial_capacity = if let Some(capacity) = minimum_capacity {
            capacity.max(256)
        } else {
            256
        };
        HashMap {
            array: Atomic::new(Array::<K, V>::new(initial_capacity, Atomic::null())),
            minimum_capacity: initial_capacity,
            resize_mutex: AtomicBool::new(false),
            hasher: hasher,
        }
    }

    /// Inserts a key-value pair into the HashMap.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.insert(1, 1);
    /// if let Err((result, value)) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    ///     assert_eq!(value, 1);
    /// }
    /// ```
    pub fn insert<'a>(
        &'a self,
        key: K,
        value: V,
    ) -> Result<Accessor<'a, K, V, H>, (Accessor<'a, K, V, H>, V)> {
        let (hash, partial_hash) = self.hash(&key);
        let mut resize_triggered = false;
        loop {
            let (mut accessor, cell_index) = self.acquire(&key, hash, partial_hash);
            if !accessor.entry_ptr.is_null() {
                return Err((accessor, value));
            }
            if !resize_triggered
                && accessor.cell_locker.full()
                && cell_index < cell::ARRAY_SIZE as usize
            {
                drop(accessor);
                self.resize(false);
                resize_triggered = true;
                continue;
            }

            let (sub_index, entry_array_link_ptr, entry_ptr) =
                accessor.cell_locker.insert(key, partial_hash, value);
            accessor.sub_index = sub_index;
            accessor.entry_array_link_ptr = entry_array_link_ptr;
            accessor.entry_ptr = entry_ptr;
            return Ok(accessor);
        }
    }

    /// Upserts a key-value pair into the HashMap.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.upsert(1, 1);
    /// assert_eq!(result.get(), (&1, &mut 1));
    /// ```
    pub fn upsert<'a>(&'a self, key: K, value: V) -> Accessor<'a, K, V, H> {
        match self.insert(key, value) {
            Ok(result) => result,
            Err((result, value)) => {
                let pair_mut_ptr = result.entry_ptr as *mut (K, V);
                unsafe { (*pair_mut_ptr).1 = value };
                result
            }
        }
    }

    /// Gets a mutable reference to the value associated with the key.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.get(1);
    /// assert_eq!(result.unwrap().get(), (&1, &mut 0));
    /// ```
    pub fn get<'a>(&'a self, key: K) -> Option<Accessor<'a, K, V, H>> {
        let (hash, partial_hash) = self.hash(&key);
        let (accessor, _) = self.acquire(&key, hash, partial_hash);
        if accessor.entry_ptr.is_null() {
            return None;
        }
        Some(accessor)
    }

    /// Removes a key-value pair.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.remove(1);
    /// assert_eq!(result, false);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.remove(1);
    /// assert!(result);
    /// ```
    pub fn remove(&self, key: K) -> bool {
        self.get(key)
            .map_or_else(|| false, |accessor| accessor.erase())
    }

    /// Reads a key-value pair.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.read(1, |key, value| *value);
    /// assert_eq!(result.unwrap(), 0);
    /// ```
    pub fn read<U, F: FnOnce(&K, &V) -> U>(&self, key: K, f: F) -> Option<U> {
        let (hash, partial_hash) = self.hash(&key);
        let guard = crossbeam_epoch::pin();

        // an acquire fence is required to correctly load the contents of the array
        let current_array = self.array.load(Acquire, &guard);
        let current_array_ref = unsafe { current_array.deref() };
        let old_array = current_array_ref.old_array(&guard);
        for array_ptr in vec![old_array.as_raw(), current_array.as_raw()] {
            if array_ptr.is_null() {
                continue;
            }
            if array_ptr == old_array.as_raw() {
                if current_array_ref.partial_rehash(&guard, |key| self.hash(key)) {
                    continue;
                }
            }
            let array_ref = unsafe { &(*array_ptr) };
            let cell_index = array_ref.calculate_cell_index(hash);
            let reader = CellReader::lock(
                array_ref.cell(cell_index),
                array_ref.entry_array(cell_index),
            );
            if let Some(entry_ptr) = reader.search(&key, partial_hash) {
                let entry_ref = unsafe { &(*entry_ptr) };
                return Some(f(&entry_ref.0, &entry_ref.1));
            }
        }
        None
    }

    /// Retains the key-value pairs that satisfy the given predicate.
    ///
    /// It returns the number of entries remaining and removed.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.insert(2, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&2, &mut 0));
    /// }
    ///
    /// let result = hashmap.retain(|key, value| *key == 1 && *value == 0);
    /// assert_eq!(result, (1, 1));
    ///
    /// let result = hashmap.get(1);
    /// assert_eq!(result.unwrap().get(), (&1, &mut 0));
    ///
    /// let result = hashmap.get(2);
    /// assert!(result.is_none());
    /// ```
    pub fn retain<F: Fn(&K, &mut V) -> bool>(&self, f: F) -> (usize, usize) {
        let mut retained_entries = 0;
        let mut removed_entries = 0;
        let mut scanner = self.iter();
        while let Some((key, value)) = scanner.next() {
            if !f(key, value) {
                scanner.erase_on_next = true;
                removed_entries += 1;
            } else {
                retained_entries += 1;
            }
        }
        if removed_entries > retained_entries {
            let guard = crossbeam_epoch::pin();
            let current_array = self.array.load(Acquire, &guard);
            let current_array_ref = unsafe { current_array.deref() };
            if retained_entries <= current_array_ref.capacity() / 8 {
                self.resize(true);
            }
        }
        (retained_entries, removed_entries)
    }

    /// Clears all the key-value pairs.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.insert(2, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&2, &mut 0));
    /// }
    ///
    /// let result = hashmap.clear();
    /// assert_eq!(result, 2);
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    ///
    /// let result = hashmap.get(2);
    /// assert!(result.is_none());
    /// ```
    pub fn clear(&self) -> usize {
        self.retain(|_, _| false).1
    }

    /// Returns an estimated size of the HashMap.
    ///
    /// It passes the capacity of the HashMap to the given function.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let result = hashmap.len(|capacity| capacity);
    /// assert_eq!(result, 1);
    ///
    /// let result = hashmap.len(|capacity| capacity / 2);
    /// assert!(result == 0 || result == 2);
    /// ```
    pub fn len<F: FnOnce(usize) -> usize>(&self, f: F) -> usize {
        let guard = crossbeam_epoch::pin();
        let current_array = self.array.load(Acquire, &guard);
        let current_array_ref = unsafe { current_array.deref() };
        let old_array = current_array_ref.old_array(&guard);
        let capacity = current_array_ref.capacity();
        let num_samples = std::cmp::min(f(capacity), capacity).next_power_of_two();
        let num_cells_to_sample = (num_samples / cell::ARRAY_SIZE as usize).max(1);
        if !old_array.is_null() {
            for _ in 0..num_cells_to_sample {
                if current_array_ref.partial_rehash(&guard, |key| self.hash(key)) {
                    break;
                }
            }
        }
        let mut num_entries = 0;
        for i in 0..num_cells_to_sample {
            let (size, linked_entries) = current_array_ref.cell(i).size();
            num_entries += size + linked_entries;
        }
        num_entries * (current_array_ref.num_cells() / num_cells_to_sample)
    }

    /// Returns the capacity of the HashMap.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), Some(1000000));
    ///
    /// let result = hashmap.capacity();
    /// assert_eq!(result, 1048576);
    /// ```
    pub fn capacity(&self) -> usize {
        let guard = crossbeam_epoch::pin();
        let current_array = self.array.load(Acquire, &guard);
        let current_array_ref = unsafe { current_array.deref() };
        if !current_array_ref.old_array(&guard).is_null() {
            current_array_ref.partial_rehash(&guard, |key| self.hash(key));
        }
        current_array_ref.capacity()
    }

    /// Returns the statistics of the HashMap.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), Some(1000));
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let statistics = hashmap.statistics();
    /// assert_eq!(statistics.num_entries(), 1);
    /// assert_eq!(statistics.capacity(), 1024);
    /// ```
    pub fn statistics(&self) -> Statistics {
        let mut statistics = Statistics {
            capacity: 0,
            effective_capacity: 0,
            cells: 0,
            killed_entries: 0,
            empty_cells: 0,
            max_consecutive_empty_cells: 0,
            entries: 0,
            linked_entries: 0,
            cells_having_link: 0,
            max_link_length: 0,
        };
        let guard = crossbeam_epoch::pin();
        let current_array = self.array.load(Acquire, &guard);
        let old_array = unsafe { current_array.deref().old_array(&guard) };
        for array_ptr in vec![old_array.as_raw(), current_array.as_raw()] {
            if array_ptr.is_null() {
                continue;
            }
            let array_ref = unsafe { &(*array_ptr) };
            let num_cells = array_ref.num_cells();
            let mut consecutive_empty_cells = 0;
            statistics.capacity += num_cells * cell::ARRAY_SIZE as usize;
            if array_ptr == current_array.as_raw() {
                statistics.effective_capacity = num_cells * cell::ARRAY_SIZE as usize;
            }
            statistics.cells += num_cells;
            for i in 0..num_cells {
                let (size, linked_entries) = array_ref.cell(i).size();
                statistics.entries += size + linked_entries;
                if size == 0 {
                    statistics.empty_cells += 1;
                    consecutive_empty_cells += 1;
                } else {
                    if statistics.max_consecutive_empty_cells < consecutive_empty_cells {
                        statistics.max_consecutive_empty_cells = consecutive_empty_cells;
                    }
                    consecutive_empty_cells = 0;
                }
                if linked_entries > 0 {
                    statistics.linked_entries += linked_entries;
                    statistics.cells_having_link += 1;
                    if statistics.max_link_length < linked_entries {
                        statistics.max_link_length = linked_entries;
                    }
                }
                if array_ref.cell(i).killed() {
                    statistics.killed_entries += 1;
                }
            }
        }
        statistics
    }

    /// Returns a Scanner.
    ///
    /// It is guaranteed to scan all the key-value pairs pertaining in the HashMap at the moment,
    /// however the same key-value pair can be scanned more than once if the HashMap is being resized.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    /// }
    ///
    /// let mut iter = hashmap.iter();
    /// assert_eq!(iter.next(), Some((&1, &mut 0)));
    /// assert_eq!(iter.next(), None);
    ///
    /// for iter in hashmap.iter() {
    ///     assert_eq!(iter, (&1, &mut 0));
    /// }
    /// ```
    pub fn iter<'a>(&'a self) -> Scanner<'a, K, V, H> {
        let (locker, array_ptr, cell_index) = self.first();
        if let Some(locker) = locker {
            if let Some(scanner) = self.pick(locker, array_ptr, cell_index) {
                return scanner;
            }
        }
        Scanner {
            accessor: None,
            array_ptr: std::ptr::null(),
            cell_index: 0,
            activated: false,
            erase_on_next: false,
        }
    }

    /// Returns a hash value of the given key.
    fn hash(&self, key: &K) -> (u64, u16) {
        // generate a hash value
        let mut h = self.hasher.build_hasher();
        key.hash(&mut h);
        let mut hash = h.finish();

        // bitmix: https://mostlymangling.blogspot.com/2019/01/better-stronger-mixer-and-test-procedure.html
        hash = hash ^ (hash.rotate_right(25) ^ hash.rotate_right(50));
        hash = hash.overflowing_mul(0xA24BAED4963EE407u64).0;
        hash = hash ^ (hash.rotate_right(24) ^ hash.rotate_right(49));
        hash = hash.overflowing_mul(0x9FB21C651E98DF25u64).0;
        hash = hash ^ (hash >> 28);
        (hash, (hash & ((1 << 16) - 1)).try_into().unwrap())
    }

    /// Acquires a cell.
    fn acquire<'a>(
        &'a self,
        key: &K,
        hash: u64,
        partial_hash: u16,
    ) -> (Accessor<'a, K, V, H>, usize) {
        let guard = crossbeam_epoch::pin();

        // it is guaranteed that the thread reads a consistent snapshot of the current and
        // old array pair by a release fence in the resize function, hence the following
        // procedure is correct.
        //  - the thread reads self.array, and it kills the target cell in the old array
        //    if there is one attached to it, and inserts the key into array.
        // There are two cases.
        //  1. the thread reads an old version of self.array.
        //    if there is another thread having read the latest version of self.array,
        //    trying to insert the same key, it will try to kill the cell in the old version
        //    of self.array, thus competing with each other.
        //  2. the thread reads the latest version of self.array.
        //    if the array is deprecated while inserting the key, it falls into case 1.
        loop {
            // an acquire fence is required to correctly load the contents of the array
            let current_array = self.array.load(Acquire, &guard);
            let current_array_ref = unsafe { current_array.deref() };
            let old_array = current_array_ref.old_array(&guard);
            if !old_array.is_null() {
                if current_array_ref.partial_rehash(&guard, |key| self.hash(key)) {
                    continue;
                }
                let (mut locker, entry_array_link_ptr, entry_ptr, cell_index, sub_index) =
                    self.search(&key, hash, partial_hash, old_array.as_raw());
                if !entry_ptr.is_null() {
                    return (
                        Accessor {
                            hash_map: &self,
                            cell_locker: locker,
                            cell_in_sampling_range: cell_index < cell::ARRAY_SIZE as usize,
                            sub_index: sub_index,
                            entry_array_link_ptr: entry_array_link_ptr,
                            entry_ptr: entry_ptr,
                        },
                        cell_index,
                    );
                } else if !locker.killed() {
                    // kill the cell
                    let old_array_ref = unsafe { old_array.deref() };
                    current_array_ref.kill_cell(&mut locker, old_array_ref, cell_index, &|key| {
                        self.hash(key)
                    });
                }
            }
            let (locker, entry_array_link_ptr, entry_ptr, cell_index, sub_index) =
                self.search(&key, hash, partial_hash, current_array.as_raw());
            if !locker.killed() {
                return (
                    Accessor {
                        hash_map: &self,
                        cell_locker: locker,
                        cell_in_sampling_range: cell_index < cell::ARRAY_SIZE as usize,
                        sub_index: sub_index,
                        entry_array_link_ptr: entry_array_link_ptr,
                        entry_ptr: entry_ptr,
                    },
                    cell_index,
                );
            }
            // reaching here indicates that self.array is updated
        }
    }

    /// Erases a key-value pair owned by the accessor.
    fn erase<'a>(&'a self, mut accessor: Accessor<'a, K, V, H>) {
        accessor.cell_locker.remove(
            true,
            accessor.sub_index,
            accessor.entry_array_link_ptr,
            accessor.entry_ptr,
        );
        if accessor.cell_in_sampling_range && accessor.cell_locker.empty() {
            drop(accessor);
            self.resize(true);
        }
    }

    /// Searches a cell for the key.
    fn search<'a>(
        &self,
        key: &K,
        hash: u64,
        partial_hash: u16,
        array_ptr: *const Array<K, V>,
    ) -> (
        CellLocker<'a, K, V>,
        *const EntryArrayLink<K, V>,
        *const (K, V),
        usize,
        u8,
    ) {
        let array_ref = unsafe { &(*array_ptr) };
        let cell_index = array_ref.calculate_cell_index(hash);
        let locker = CellLocker::lock(
            array_ref.cell(cell_index),
            array_ref.entry_array(cell_index),
        );
        if !locker.killed() && !locker.empty() {
            if let Some((sub_index, entry_array_link_ptr, entry_ptr)) =
                locker.search(key, partial_hash)
            {
                return (
                    locker,
                    entry_array_link_ptr,
                    entry_ptr,
                    cell_index,
                    sub_index,
                );
            }
        }
        (locker, std::ptr::null(), std::ptr::null(), cell_index, 0)
    }

    /// Returns the first valid cell.
    fn first<'a>(&'a self) -> (Option<CellLocker<'a, K, V>>, *const Array<K, V>, usize) {
        let guard = crossbeam_epoch::pin();

        // an acquire fence is required to correctly load the contents of the array
        let mut current_array = self.array.load(Acquire, &guard);
        loop {
            let old_array = unsafe { current_array.deref().old_array(&guard) };
            for array_ptr in vec![old_array.as_raw(), current_array.as_raw()] {
                if array_ptr.is_null() {
                    continue;
                }
                let array_ref = unsafe { &(*array_ptr) };
                let num_cells = array_ref.num_cells();
                for cell_index in 0..num_cells {
                    let locker = CellLocker::lock(
                        array_ref.cell(cell_index),
                        array_ref.entry_array(cell_index),
                    );
                    if !locker.empty() {
                        // once a valid cell is locked, the array is guaranteed to retain
                        return (Some(locker), array_ptr, cell_index);
                    }
                }
            }
            // no valid cells found
            let current_array_new = self.array.load(Acquire, &guard);
            if current_array == current_array_new {
                break;
            }

            // resized in the meantime
            current_array = current_array_new;
        }
        (None, std::ptr::null(), 0)
    }

    /// Returns the next valid cell.
    fn next<'a>(
        &'a self,
        array_ptr: *const Array<K, V>,
        current_index: usize,
    ) -> Option<Scanner<'a, K, V, H>> {
        let guard = crossbeam_epoch::pin();

        // an acquire fence is required to correctly load the contents of the array
        let current_array = self.array.load(Acquire, &guard);
        // bypass the lifetime checker by not calling Shared::deref()
        let current_array_ref = unsafe { &(*current_array.as_raw()) };
        let old_array = current_array_ref.old_array(&guard);

        // either one of the two arrays must match with array_ptr
        debug_assert!(array_ptr == current_array.as_raw() || array_ptr == old_array.as_raw());

        if old_array.as_raw() == array_ptr {
            // bypass the lifetime checker by not calling Shared::deref()
            let old_array_ref = unsafe { &(*old_array.as_raw()) };
            let num_cells = old_array_ref.num_cells();
            for cell_index in (current_index + 1)..num_cells {
                let locker = CellLocker::lock(
                    old_array_ref.cell(cell_index),
                    old_array_ref.entry_array(cell_index),
                );
                if !locker.killed() && !locker.empty() {
                    if let Some(scanner) = self.pick(locker, old_array.as_raw(), cell_index) {
                        return Some(scanner);
                    }
                }
            }
        }

        let mut new_array = Shared::<Array<K, V>>::null();
        let num_cells = current_array_ref.num_cells();
        let start_index = if old_array.as_raw() == array_ptr {
            0
        } else {
            current_index + 1
        };
        for cell_index in (start_index)..num_cells {
            let locker = CellLocker::lock(
                current_array_ref.cell(cell_index),
                current_array_ref.entry_array(cell_index),
            );
            if !locker.killed() && !locker.empty() {
                if let Some(scanner) = self.pick(locker, current_array.as_raw(), cell_index) {
                    return Some(scanner);
                }
            } else if locker.killed() && new_array.is_null() {
                new_array = self.array.load(Acquire, &guard);
            }
        }

        if !new_array.is_null() {
            // bypass the lifetime checker by not calling Shared::deref()
            let new_array_ref = unsafe { &(*new_array.as_raw()) };
            let num_cells = new_array_ref.num_cells();
            for cell_index in 0..num_cells {
                let locker = CellLocker::lock(
                    new_array_ref.cell(cell_index),
                    new_array_ref.entry_array(cell_index),
                );
                if !locker.killed() && !locker.empty() {
                    if let Some(scanner) = self.pick(locker, new_array.as_raw(), cell_index) {
                        return Some(scanner);
                    }
                }
            }
        }
        None
    }

    /// Picks a key-value pair entry using the given CellLocker.
    fn pick<'a>(
        &'a self,
        cell_locker: CellLocker<'a, K, V>,
        array_ptr: *const Array<K, V>,
        cell_index: usize,
    ) -> Option<Scanner<'a, K, V, H>> {
        if let Some((sub_index, entry_array_link_ptr, entry_ptr)) = cell_locker.first() {
            return Some(Scanner {
                accessor: Some(Accessor {
                    hash_map: &self,
                    cell_locker: cell_locker,
                    cell_in_sampling_range: cell_index < cell::ARRAY_SIZE as usize,
                    sub_index: sub_index,
                    entry_array_link_ptr: entry_array_link_ptr,
                    entry_ptr: entry_ptr,
                }),
                array_ptr: array_ptr,
                cell_index: cell_index,
                activated: false,
                erase_on_next: false,
            });
        }
        None
    }

    /// Resizes the array
    fn resize(&self, shrink: bool) {
        // initial rough size estimation using a small number of cells
        let guard = crossbeam_epoch::pin();
        let current_array = self.array.load(Acquire, &guard);
        let current_array_ref = unsafe { current_array.deref() };
        let old_array = current_array_ref.old_array(&guard);
        if !old_array.is_null() {
            if !current_array_ref.partial_rehash(&guard, |key| self.hash(key)) {
                return;
            }
        } else if shrink && current_array_ref.capacity() == self.minimum_capacity {
            return;
        }

        // trigger resize if the sampling entries have less than 1/16, or more than 15/16 entries
        let mut num_entries = 0;
        for i in 0..current_array_ref.num_cells().min(cell::ARRAY_SIZE as usize) {
            num_entries += current_array_ref.cell(i).size().0;
            if shrink {
                if num_entries >= cell::ARRAY_SIZE as usize {
                    return;
                }
            } else {
                if ((i + 1) * cell::ARRAY_SIZE as usize) - num_entries >= cell::ARRAY_SIZE as usize
                {
                    return;
                }
            }
        }

        // resize
        if !self.resize_mutex.swap(true, Acquire) {
            if current_array != self.array.load(Acquire, &guard) {
                self.resize_mutex.store(false, Release);
                return;
            }

            // the resizing policies are as follows.
            //  - load factor reaches 7/8: enlarge up to 64x
            //  - load factor reaches 1/8: shrink
            let capacity = current_array_ref.capacity();
            let estimated_num_entries = self.len(|capacity| (capacity / 16).min(16384));
            let new_capacity = if estimated_num_entries >= (capacity / 8) * 7 {
                if capacity >= (1usize << (std::mem::size_of::<usize>() * 8 - 1)) {
                    capacity
                } else {
                    (capacity.min(
                        1usize
                            << (std::mem::size_of::<usize>() * 8
                                - (MAX_ENLARGE_FACTOR as usize + 1)),
                    ) * (1 << MAX_ENLARGE_FACTOR as usize))
                        .min(estimated_num_entries.next_power_of_two() * 2)
                }
            } else if estimated_num_entries <= capacity / 8 {
                estimated_num_entries
                    .next_power_of_two()
                    .max(self.minimum_capacity)
            } else {
                capacity
            };

            // Array::new may not be able to allocate the requested number of cells
            if new_capacity != capacity {
                let new_array = Array::<K, V>::new(new_capacity, Atomic::from(current_array));
                if (!shrink && new_array.capacity() > capacity)
                    || (shrink && new_array.capacity() == new_capacity)
                {
                    self.array.store(Owned::new(new_array), Release);
                }
            }

            self.resize_mutex.store(false, Release);
        }
    }
}

impl<K: Eq + Hash + Sync, V: Sync, H: BuildHasher> Drop for HashMap<K, V, H> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Accessor owns a key-value pair in the HashMap.
///
/// It is !Send, thus disallowing other threads to have references to it.
/// It acquires an exclusive lock on the cell managing the key.
/// Instantiating multiple Accessor of Scanner instances in a thread poses a possibility of deadlock.
pub struct Accessor<'a, K: Eq + Hash + Sync, V: Sync, H: BuildHasher> {
    hash_map: &'a HashMap<K, V, H>,
    cell_locker: CellLocker<'a, K, V>,
    cell_in_sampling_range: bool,
    sub_index: u8,
    entry_array_link_ptr: *const EntryArrayLink<K, V>,
    entry_ptr: *const (K, V),
}

impl<'a, K: Eq + Hash + Sync, V: Sync, H: BuildHasher> Accessor<'a, K, V, H> {
    /// Returns a reference to the key-value pair.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    ///     (*result.get().1) = 2;
    /// }
    ///
    /// let result = hashmap.get(1);
    /// assert_eq!(result.unwrap().get(), (&1, &mut 2));
    /// ```
    pub fn get(&'a self) -> (&'a K, &'a mut V) {
        unsafe {
            let key_ptr = &(*self.entry_ptr).0 as *const K;
            let value_ptr = &(*self.entry_ptr).1 as *const V;
            let value_mut_ptr = value_ptr as *mut V;
            (&(*key_ptr), &mut (*value_mut_ptr))
        }
    }

    /// Erases the key-value pair owned by the Accessor.
    ///
    /// # Examples
    /// ```
    /// use scc::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), None);
    ///
    /// let result = hashmap.insert(1, 0);
    /// if let Ok(result) = result {
    ///     assert_eq!(result.get(), (&1, &mut 0));
    ///     result.erase();
    /// }
    ///
    /// let result = hashmap.get(1);
    /// assert!(result.is_none());
    /// ```
    pub fn erase(self) -> bool {
        if self.entry_ptr.is_null() {
            return false;
        }
        self.hash_map.erase(self);
        true
    }
}

/// Scanner implements Iterator.
///
/// It is !Send, thus disallowing other threads to have references to it.
/// It acquires an exclusive lock on a cell that is currently being scanned.
/// Instantiating multiple Accessor of Scanner instances in a thread poses a possibility of deadlock.
pub struct Scanner<'a, K: Eq + Hash + Sync, V: Sync, H: BuildHasher> {
    accessor: Option<Accessor<'a, K, V, H>>,
    array_ptr: *const Array<K, V>,
    cell_index: usize,
    activated: bool,
    erase_on_next: bool,
}

impl<'a, K: Eq + Hash + Sync, V: Sync, H: BuildHasher> Iterator for Scanner<'a, K, V, H> {
    type Item = (&'a K, &'a mut V);
    fn next(&mut self) -> Option<Self::Item> {
        if !self.activated {
            self.activated = true;
        } else if self.accessor.is_some() {
            let erase = self.erase_on_next;
            if erase {
                self.erase_on_next = false;
            }
            if let Some((next_sub_index, next_entry_array_link_ptr, next_entry_ptr)) =
                self.accessor.as_mut().map_or_else(
                    || None,
                    |accessor| {
                        accessor.cell_locker.next(
                            erase,
                            true,
                            accessor.sub_index,
                            accessor.entry_array_link_ptr,
                            accessor.entry_ptr,
                        )
                    },
                )
            {
                self.accessor.as_mut().map_or_else(
                    || (),
                    |accessor| {
                        accessor.sub_index = next_sub_index;
                        accessor.entry_array_link_ptr = next_entry_array_link_ptr;
                        accessor.entry_ptr = next_entry_ptr;
                    },
                );
            } else {
                let current_array_ptr = self.array_ptr;
                let current_cell_index = self.cell_index;
                let scanner = self.accessor.as_ref().map_or_else(
                    || None,
                    |accessor| {
                        accessor
                            .hash_map
                            .next(current_array_ptr, current_cell_index)
                    },
                );
                self.accessor.take();
                if let Some(mut scanner) = scanner {
                    self.accessor = scanner.accessor.take();
                    self.array_ptr = scanner.array_ptr;
                    self.cell_index = scanner.cell_index;
                }
            }
        }
        if let Some(accessor) = &self.accessor {
            unsafe {
                let key_ptr = &(*accessor.entry_ptr).0 as *const K;
                let value_ptr = &(*accessor.entry_ptr).1 as *const V;
                let value_mut_ptr = value_ptr as *mut V;
                return Some((&(*key_ptr), &mut (*value_mut_ptr)));
            }
        }
        None
    }
}

/// Statistics
pub struct Statistics {
    capacity: usize,
    effective_capacity: usize,
    entries: usize,
    killed_entries: usize,
    cells: usize,
    empty_cells: usize,
    max_consecutive_empty_cells: usize,
    linked_entries: usize,
    cells_having_link: usize,
    max_link_length: usize,
}

impl Statistics {
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn effective_capacity(&self) -> usize {
        self.effective_capacity
    }
    pub fn num_entries(&self) -> usize {
        self.entries
    }
    pub fn num_killed_entries(&self) -> usize {
        self.killed_entries
    }
    pub fn num_cells(&self) -> usize {
        self.cells
    }
    pub fn num_empty_cells(&self) -> usize {
        self.empty_cells
    }
    pub fn max_consecutive_empty_cells(&self) -> usize {
        self.max_consecutive_empty_cells
    }
    pub fn num_linked_entries(&self) -> usize {
        self.linked_entries
    }
    pub fn num_cells_having_link(&self) -> usize {
        self.cells_having_link
    }
    pub fn max_link_length(&self) -> usize {
        self.max_link_length
    }
}

impl fmt::Display for Statistics {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "capacity: {}, effective_capacity: {}, cells: {}, killed_entries: {}, empty_cells: {}, max_consecutive_empty_cells: {}, entries: {}, linked_entries: {}, cells_having_link: {}, max_link_length: {}",
            self.capacity,
            self.effective_capacity,
            self.cells,
            self.killed_entries,
            self.empty_cells,
            self.max_consecutive_empty_cells,
            self.entries,
            self.linked_entries,
            self.cells_having_link,
            self.max_link_length
        )
    }
}
