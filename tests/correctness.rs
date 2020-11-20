use proptest::proptest;
use scc::HashMap;
use std::collections::hash_map::RandomState;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::Arc;
use std::thread;

proptest! {
    #[test]
    fn basic_hashmap(key in 0u64..10) {
        let hashmap: HashMap<u64, u32, RandomState> = HashMap::new(RandomState::new(), Some(10));
        assert!(hashmap.iter().next().is_none());

        let result1 = hashmap.insert(key, 0);
        assert!(result1.is_ok());
        if let Ok(result) = result1 {
            assert_eq!(result.get(), (&key, &mut 0));
        }

        let result2 = hashmap.insert(key, 0);
        assert!(result2.is_err());
        if let Err((result, _)) = result2 {
            assert_eq!(result.get(), (&key, &mut 0));
        }

        let result3 = hashmap.upsert(key, 1);
        assert_eq!(result3.get(), (&key, &mut 1));
        drop(result3);

        let result4 = hashmap.insert(key, 10);
        assert!(result4.is_err());
        if let Err((result, _)) = result4 {
            assert_eq!(result.get(), (&key, &mut 1));
            *result.get().1 = 2;
        }

        let mut result5 = hashmap.iter();
        assert_eq!(result5.next(), Some((&key, &mut 2)));
        assert_eq!(result5.next(), None);

        for iter in hashmap.iter() {
            assert_eq!(iter, (&key, &mut 2));
            *iter.1 = 3;
        }

        let result6 = hashmap.get(key);
        assert_eq!(result6.unwrap().get(), (&key, &mut 3));

        let result7 = hashmap.get(key + 1);
        assert!(result7.is_none());

        let result8 = hashmap.remove(key);
        assert_eq!(result8, true);

        let result9 = hashmap.insert(key + 2, 10);
        assert!(result9.is_ok());
        if let Ok(result) = result9 {
            assert_eq!(result.get(), (&(key + 2), &mut 10));
            result.erase();
        }

        let result10 = hashmap.get(key + 2);
        assert!(result10.is_none());
    }
}

#[test]
fn basic_scanner() {
    for _ in 0..64 {
        let hashmap: Arc<HashMap<u64, u64, RandomState>> =
            Arc::new(HashMap::new(RandomState::new(), None));
        let hashmap_copied = hashmap.clone();
        let inserted = Arc::new(AtomicU64::new(0));
        let inserted_copied = inserted.clone();
        let thread_handle = thread::spawn(move || {
            for _ in 0..16 {
                let mut scanned = 0;
                let mut checker = BTreeSet::new();
                let max = inserted_copied.load(Acquire);
                for iter in hashmap_copied.iter() {
                    scanned += 1;
                    checker.insert(*iter.0);
                }
                println!("scanned: {}, max: {}", scanned, max);
                for key in 0..max {
                    assert!(checker.contains(&key));
                }
            }
        });
        for i in 0..16384 {
            assert!(hashmap.insert(i, i).is_ok());
            inserted.store(i, Release);
        }
        thread_handle.join().unwrap();
    }
}

struct Data<'a> {
    data: u64,
    checker: &'a AtomicUsize,
}

impl<'a> Data<'a> {
    fn new(data: u64, checker: &'a AtomicUsize) -> Data<'a> {
        checker.fetch_add(1, Relaxed);
        Data {
            data: data,
            checker: checker,
        }
    }
}

impl<'a> Clone for Data<'a> {
    fn clone(&self) -> Self {
        Data::new(self.data, self.checker)
    }
}

impl<'a> Drop for Data<'a> {
    fn drop(&mut self) {
        self.checker.fetch_sub(1, Relaxed);
    }
}

impl<'a> Eq for Data<'a> {}

impl<'a> Hash for Data<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.data.hash(state);
    }
}

impl<'a> PartialEq for Data<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

proptest! {
    #[test]
    fn insert(key in 0u64..16) {
        let range = 1024;
        let checker = AtomicUsize::new(0);
        let hashmap: HashMap<Data, Data, RandomState> = HashMap::new(RandomState::new(), Some(10));
        for d in key..(key + range) {
            let result = hashmap.insert(Data::new(d, &checker), Data::new(d, &checker));
            assert!(result.is_ok());
            drop(result);
            let result = hashmap.upsert(Data::new(d, &checker), Data::new(d + 1, &checker));
            (*result.get().1) = Data::new(d + 2, &checker);
        }
        let statistics = hashmap.statistics();
        println!("{}", statistics);

        for d in (key + range)..(key + range + range) {
            let result = hashmap.insert(Data::new(d, &checker), Data::new(d, &checker));
            assert!(result.is_ok());
            drop(result);
            let result = hashmap.upsert(Data::new(d, &checker), Data::new(d + 1, &checker));
            (*result.get().1) = Data::new(d + 2, &checker);
        }
        let statistics = hashmap.statistics();
        println!("before retain: {}", statistics);

        let result = hashmap.retain(|k, _| k.data < key + range);
        assert_eq!(result, (range as usize, range as usize));

        let statistics = hashmap.statistics();
        println!("after retain: {}", statistics);

        assert_eq!(statistics.num_entries() as u64, range);
        let mut found_keys = 0;
        for iter in hashmap.iter() {
            assert!(iter.0.data < key + range);
            assert!(iter.0.data >= key);
            found_keys += 1;
        }
        assert_eq!(found_keys, range);
        assert_eq!(checker.load(Relaxed) as u64, range * 2);
        for d in key..(key + range) {
            let result = hashmap.get(Data::new(d, &checker));
            result.unwrap().erase();
        }
        assert_eq!(checker.load(Relaxed), 0);

        let statistics = hashmap.statistics();
        println!("after erase: {}", statistics);

        for d in key..(key + range) {
            let result = hashmap.insert(Data::new(d, &checker), Data::new(d, &checker));
            assert!(result.is_ok());
            drop(result);
            let result = hashmap.upsert(Data::new(d, &checker), Data::new(d + 1, &checker));
            (*result.get().1) = Data::new(d + 2, &checker);
        }
        let result = hashmap.clear();
        assert_eq!(result, range as usize);
        assert_eq!(checker.load(Relaxed), 0);

        let statistics = hashmap.statistics();
        println!("after clear: {}", statistics);
    }
}

#[test]
fn sample() {
    for s in vec![65536, 2097152, 16777216] {
        let hashmap: HashMap<usize, u8, RandomState> = HashMap::new(RandomState::new(), Some(s));
        let step_size = s / 10;
        for p in 0..10 {
            for i in (p * step_size)..((p + 1) * step_size) {
                assert!(hashmap.insert(i, 0).is_ok());
            }
            let statistics = hashmap.statistics();
            println!("{}%: {}", (p + 1) * 10, statistics);
            for sample_size in 0..9 {
                let len = hashmap.len(|_| (1 << sample_size) * 16);
                println!("{}/{}%: {};{}", s, (p + 1) * 10, 1 << sample_size, len);
            }
        }
    }
}
