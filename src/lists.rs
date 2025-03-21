use std::ops::Range;

use crate::{
    hashing::PartedHash,
    shard::{InsertMode, KVPair},
    store::{CHAIN_NAMESPACE, ITEM_NAMESPACE, LIST_NAMESPACE},
    CandyStore, GetOrCreateStatus, ReplaceStatus, Result, SetStatus,
};

use bytemuck::{bytes_of, from_bytes, Pod, Zeroable};
use parking_lot::MutexGuard;

#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct List {
    head_idx: u64, // inclusive
    tail_idx: u64, // exclusive
    num_items: u64,
}

impl std::fmt::Debug for List {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "List(0x{:016x}..0x{:016x} items={})",
            self.head_idx, self.tail_idx, self.num_items
        )
    }
}

impl List {
    fn span_len(&self) -> u64 {
        self.tail_idx - self.head_idx
    }
    fn holes(&self) -> u64 {
        self.span_len() - self.num_items
    }
    fn is_empty(&self) -> bool {
        self.head_idx == self.tail_idx
    }
}

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C, packed)]
struct ChainKey {
    list_ph: PartedHash,
    idx: u64,
    namespace: u8,
}

#[derive(Debug)]
pub struct ListCompactionParams {
    pub min_length: u64,
    pub min_holes_ratio: f64,
}

impl Default for ListCompactionParams {
    fn default() -> Self {
        Self {
            min_length: 100,
            min_holes_ratio: 0.25,
        }
    }
}

pub struct ListIterator<'a> {
    store: &'a CandyStore,
    list_key: Vec<u8>,
    list_ph: PartedHash,
    range: Option<Range<u64>>,
    fwd: bool,
}

impl<'a> Iterator for ListIterator<'a> {
    type Item = Result<KVPair>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.is_none() {
            let _guard = self.store.lock_list(self.list_ph);
            let list_bytes = match self.store.get_raw(&self.list_key) {
                Ok(Some(list_bytes)) => list_bytes,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            let list = *from_bytes::<List>(&list_bytes);
            self.range = Some(list.head_idx..list.tail_idx);
        }

        loop {
            let idx = if self.fwd {
                self.range.as_mut().unwrap().next()
            } else {
                self.range.as_mut().unwrap().next_back()
            };
            let Some(idx) = idx else {
                return None;
            };

            match self.store.get_from_list_at_index(self.list_ph, idx, true) {
                Err(e) => return Some(Err(e)),
                Ok(Some((_, k, v))) => return Some(Ok((k, v))),
                Ok(None) => {
                    // try next index
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if let Some(ref range) = self.range {
            range.size_hint()
        } else {
            (0, None)
        }
    }
}

#[derive(Debug)]
enum InsertToListStatus {
    Created(Vec<u8>),
    DoesNotExist,
    WrongValue(Vec<u8>),
    ExistingValue(Vec<u8>),
    Replaced(Vec<u8>),
}

impl CandyStore {
    const FIRST_LIST_IDX: u64 = 0x8000_0000_0000_0000;

    fn make_list_key(&self, mut list_key: Vec<u8>) -> (PartedHash, Vec<u8>) {
        list_key.extend_from_slice(LIST_NAMESPACE);
        (PartedHash::new(&self.config.hash_seed, &list_key), list_key)
    }

    fn make_item_key(&self, list_ph: PartedHash, mut item_key: Vec<u8>) -> (PartedHash, Vec<u8>) {
        item_key.extend_from_slice(bytes_of(&list_ph));
        item_key.extend_from_slice(ITEM_NAMESPACE);
        (PartedHash::new(&self.config.hash_seed, &item_key), item_key)
    }

    pub(crate) fn lock_list(&self, list_ph: PartedHash) -> MutexGuard<()> {
        self.keyed_locks[(list_ph.signature() & self.keyed_locks_mask) as usize].lock()
    }

    fn _insert_to_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        mut val: Vec<u8>,
        mode: InsertMode,
    ) -> Result<InsertToListStatus> {
        let (list_ph, list_key) = self.make_list_key(list_key);
        let (item_ph, item_key) = self.make_item_key(list_ph, item_key);

        let _guard = self.lock_list(list_ph);

        // if the item already exists, it's already part of the list. just update it and preserve the index
        if let Some(mut existing_val) = self.get_raw(&item_key)? {
            match mode {
                InsertMode::GetOrCreate => {
                    existing_val.truncate(existing_val.len() - size_of::<u64>());
                    return Ok(InsertToListStatus::ExistingValue(existing_val));
                }
                InsertMode::Replace(expected_val) => {
                    if let Some(expected_val) = expected_val {
                        if expected_val != &existing_val[existing_val.len() - size_of::<u64>()..] {
                            existing_val.truncate(existing_val.len() - size_of::<u64>());
                            return Ok(InsertToListStatus::WrongValue(existing_val));
                        }
                    }
                    // fall through
                }
                InsertMode::Set => {
                    // fall through
                }
            }

            val.extend_from_slice(&existing_val[existing_val.len() - size_of::<u64>()..]);
            self.replace_raw(&item_key, &val, None)?;
            existing_val.truncate(existing_val.len() - size_of::<u64>());
            return Ok(InsertToListStatus::Replaced(existing_val));
        }

        if matches!(mode, InsertMode::Replace(_)) {
            // not allowed to create
            return Ok(InsertToListStatus::DoesNotExist);
        }

        // get of create the list
        let res = self.get_or_create_raw(
            &list_key,
            bytes_of(&List {
                head_idx: Self::FIRST_LIST_IDX,
                tail_idx: Self::FIRST_LIST_IDX + 1,
                num_items: 1,
            })
            .to_owned(),
        )?;

        match res {
            crate::GetOrCreateStatus::CreatedNew(_) => {
                // list was just created. create chain
                self.set_raw(
                    bytes_of(&ChainKey {
                        list_ph,
                        idx: Self::FIRST_LIST_IDX,
                        namespace: CHAIN_NAMESPACE,
                    }),
                    bytes_of(&item_ph),
                )?;

                // create item
                val.extend_from_slice(bytes_of(&Self::FIRST_LIST_IDX));
                self.set_raw(&item_key, &val)?;
            }
            crate::GetOrCreateStatus::ExistingValue(list_bytes) => {
                let mut list = *from_bytes::<List>(&list_bytes);

                let idx = list.tail_idx;
                list.tail_idx += 1;

                // update list
                list.num_items += 1;
                self.set_raw(&list_key, bytes_of(&list))?;

                // create chain
                self.set_raw(
                    bytes_of(&ChainKey {
                        list_ph,
                        idx,
                        namespace: CHAIN_NAMESPACE,
                    }),
                    bytes_of(&item_ph),
                )?;

                // create item
                val.extend_from_slice(bytes_of(&idx));
                self.set_raw(&item_key, &val)?;
            }
        }

        val.truncate(val.len() - size_of::<u64>());
        Ok(InsertToListStatus::Created(val))
    }

    /// Inserts or updates an element `item_key` that belongs to list `list_key`. Returns [SetStatus::CreatedNew] if
    /// the item did not exist, or [SetStatus::PrevValue] with the previous value of the item.
    ///
    /// See also [Self::set].
    pub fn set_in_list<
        B1: AsRef<[u8]> + ?Sized,
        B2: AsRef<[u8]> + ?Sized,
        B3: AsRef<[u8]> + ?Sized,
    >(
        &self,
        list_key: &B1,
        item_key: &B2,
        val: &B3,
    ) -> Result<SetStatus> {
        self.owned_set_in_list(
            list_key.as_ref().to_owned(),
            item_key.as_ref().to_owned(),
            val.as_ref().to_owned(),
            false,
        )
    }

    /// Like [Self::set_in_list] but "promotes" the element to the tail of the list: it's basically a
    /// remove + insert operation. This can be usede to implement LRUs, where older elements are at the
    /// beginning and newer ones at the end.
    ///
    /// Note: **not crash-safe**
    pub fn set_in_list_promoting<
        B1: AsRef<[u8]> + ?Sized,
        B2: AsRef<[u8]> + ?Sized,
        B3: AsRef<[u8]> + ?Sized,
    >(
        &self,
        list_key: &B1,
        item_key: &B2,
        val: &B3,
    ) -> Result<SetStatus> {
        self.owned_set_in_list(
            list_key.as_ref().to_owned(),
            item_key.as_ref().to_owned(),
            val.as_ref().to_owned(),
            true,
        )
    }

    /// Owned version of [Self::set_in_list], which also takes promote as a parameter
    pub fn owned_set_in_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        val: Vec<u8>,
        promote: bool,
    ) -> Result<SetStatus> {
        if promote {
            self.owned_remove_from_list(list_key.clone(), item_key.clone())?;
        }
        match self._insert_to_list(list_key, item_key, val, InsertMode::Set)? {
            InsertToListStatus::Created(_v) => Ok(SetStatus::CreatedNew),
            InsertToListStatus::Replaced(v) => Ok(SetStatus::PrevValue(v)),
            _ => unreachable!(),
        }
    }

    /// Like [Self::set_in_list], but will only replace (update) an existing item, i.e., it will never create the
    /// key
    pub fn replace_in_list<
        B1: AsRef<[u8]> + ?Sized,
        B2: AsRef<[u8]> + ?Sized,
        B3: AsRef<[u8]> + ?Sized,
    >(
        &self,
        list_key: &B1,
        item_key: &B2,
        val: &B3,
        expected_val: Option<&B3>,
    ) -> Result<ReplaceStatus> {
        self.owned_replace_in_list(
            list_key.as_ref().to_owned(),
            item_key.as_ref().to_owned(),
            val.as_ref().to_owned(),
            expected_val.map(|ev| ev.as_ref()),
        )
    }

    /// Owned version of [Self::replace_in_list]
    pub fn owned_replace_in_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        val: Vec<u8>,
        expected_val: Option<&[u8]>,
    ) -> Result<ReplaceStatus> {
        match self._insert_to_list(list_key, item_key, val, InsertMode::Replace(expected_val))? {
            InsertToListStatus::DoesNotExist => Ok(ReplaceStatus::DoesNotExist),
            InsertToListStatus::Replaced(v) => Ok(ReplaceStatus::PrevValue(v)),
            InsertToListStatus::WrongValue(v) => Ok(ReplaceStatus::WrongValue(v)),
            _ => unreachable!(),
        }
    }

    /// Like [Self::set_in_list] but will not replace (update) the element if it already exists - it will only
    /// create the element with the default value if it did not exist.
    pub fn get_or_create_in_list<
        B1: AsRef<[u8]> + ?Sized,
        B2: AsRef<[u8]> + ?Sized,
        B3: AsRef<[u8]> + ?Sized,
    >(
        &self,
        list_key: &B1,
        item_key: &B2,
        default_val: &B3,
    ) -> Result<GetOrCreateStatus> {
        self.owned_get_or_create_in_list(
            list_key.as_ref().to_owned(),
            item_key.as_ref().to_owned(),
            default_val.as_ref().to_owned(),
        )
    }

    /// Owned version of [Self::get_or_create_in_list]
    pub fn owned_get_or_create_in_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        default_val: Vec<u8>,
    ) -> Result<GetOrCreateStatus> {
        match self._insert_to_list(list_key, item_key, default_val, InsertMode::GetOrCreate)? {
            InsertToListStatus::ExistingValue(v) => Ok(GetOrCreateStatus::ExistingValue(v)),
            InsertToListStatus::Created(v) => Ok(GetOrCreateStatus::CreatedNew(v)),
            _ => unreachable!(),
        }
    }

    /// Gets a list element identified by `list_key` and `item_key`. This is an O(1) operation.
    ///
    /// See also: [Self::get]
    pub fn get_from_list<B1: AsRef<[u8]> + ?Sized, B2: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B1,
        item_key: &B2,
    ) -> Result<Option<Vec<u8>>> {
        self.owned_get_from_list(list_key.as_ref().to_owned(), item_key.as_ref().to_owned())
    }

    /// Owned version of [Self::get_from_list]
    pub fn owned_get_from_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let (list_ph, _) = self.make_list_key(list_key);
        let (_, item_key) = self.make_item_key(list_ph, item_key);
        let Some(mut val) = self.get_raw(&item_key)? else {
            return Ok(None);
        };
        val.truncate(val.len() - size_of::<u64>());
        Ok(Some(val))
    }

    /// Removes a element from the list, identified by `list_key` and `item_key. The element can be
    /// at any position in the list, not just the head or the tail, but in this case, it will create a "hole".
    /// This means that iterations will go over the missing element's index every time, until the list is compacted.
    ///
    /// See also [Self::remove], [Self::compact_list_if_needed]
    pub fn remove_from_list<B1: AsRef<[u8]> + ?Sized, B2: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B1,
        item_key: &B2,
    ) -> Result<Option<Vec<u8>>> {
        self.owned_remove_from_list(list_key.as_ref().to_owned(), item_key.as_ref().to_owned())
    }

    /// Owned version of [Self::remove_from_list]
    pub fn owned_remove_from_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let (list_ph, list_key) = self.make_list_key(list_key);
        let (_, item_key) = self.make_item_key(list_ph, item_key);

        let _guard = self.lock_list(list_ph);

        let Some(mut existing_val) = self.get_raw(&item_key)? else {
            return Ok(None);
        };

        let item_idx = u64::from_le_bytes(
            (&existing_val[existing_val.len() - size_of::<u64>()..])
                .try_into()
                .unwrap(),
        );
        existing_val.truncate(existing_val.len() - size_of::<u64>());

        // update list, if the item was the head/tail
        if let Some(list_bytes) = self.get_raw(&list_key)? {
            let mut list = *from_bytes::<List>(&list_bytes);

            list.num_items -= 1;

            if list.head_idx == item_idx || list.tail_idx == item_idx + 1 {
                if list.head_idx == item_idx {
                    list.head_idx += 1;
                } else if list.tail_idx == item_idx + 1 {
                    list.tail_idx -= 1;
                }
            }
            if list.is_empty() {
                self.remove_raw(&list_key)?;
            } else {
                self.set_raw(&list_key, bytes_of(&list))?;
            }
        }

        // remove chain
        self.remove_raw(bytes_of(&ChainKey {
            list_ph,
            idx: item_idx,
            namespace: CHAIN_NAMESPACE,
        }))?;

        // remove item
        self.remove_raw(&item_key)?;

        Ok(Some(existing_val))
    }

    const LIST_KEY_SUFFIX_LEN: usize = size_of::<PartedHash>() + ITEM_NAMESPACE.len();

    fn get_from_list_at_index(
        &self,
        list_ph: PartedHash,
        idx: u64,
        truncate: bool,
    ) -> Result<Option<(PartedHash, Vec<u8>, Vec<u8>)>> {
        let Some(item_ph_bytes) = self.get_raw(bytes_of(&ChainKey {
            idx,
            list_ph,
            namespace: CHAIN_NAMESPACE,
        }))?
        else {
            return Ok(None);
        };
        let item_ph = *from_bytes::<PartedHash>(&item_ph_bytes);

        let mut suffix = [0u8; Self::LIST_KEY_SUFFIX_LEN];
        suffix[0..size_of::<PartedHash>()].copy_from_slice(bytes_of(&list_ph));
        suffix[size_of::<PartedHash>()..].copy_from_slice(ITEM_NAMESPACE);

        for (mut k, mut v) in self.get_by_hash(item_ph)? {
            if k.ends_with(&suffix) && v.ends_with(bytes_of(&idx)) {
                if truncate {
                    v.truncate(v.len() - size_of::<u64>());
                    k.truncate(k.len() - suffix.len());
                }
                return Ok(Some((item_ph, k, v)));
            }
        }

        Ok(None)
    }

    /// Compacts (rewrites) the list such that there will be no holes. Holes are created when removing an
    /// element from the middle of the list (not the head or tail), which makes iteration less efficient.
    /// You should call this function every so often if you're removing elements from lists at random locations.
    /// The function takes parameters that control when to compact: the list has to be of a minimal length and
    /// have a minimal holes-to-length ratio. The default values are expected to be okay for most use cases.
    /// Returns true if the list was compacted, false otherwise.
    ///
    /// Note: **Not crash-safe**
    pub fn compact_list_if_needed<B: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B,
        params: ListCompactionParams,
    ) -> Result<bool> {
        let (list_ph, list_key) = self.make_list_key(list_key.as_ref().to_owned());
        let _guard = self.lock_list(list_ph);

        let Some(list_bytes) = self.get_raw(&list_key)? else {
            return Ok(false);
        };
        let list = *from_bytes::<List>(&list_bytes);
        if list.span_len() < params.min_length {
            return Ok(false);
        }
        if (list.holes() as f64) < (list.span_len() as f64) * params.min_holes_ratio {
            return Ok(false);
        }

        let mut new_idx = list.tail_idx;
        for idx in list.head_idx..list.tail_idx {
            let Some((item_ph, full_k, mut full_v)) =
                self.get_from_list_at_index(list_ph, idx, false)?
            else {
                continue;
            };

            // create new chain
            self.set_raw(
                bytes_of(&ChainKey {
                    idx: new_idx,
                    list_ph,
                    namespace: CHAIN_NAMESPACE,
                }),
                bytes_of(&item_ph),
            )?;

            // update item's index suffix
            let offset = full_v.len() - size_of::<u64>();
            full_v[offset..].copy_from_slice(bytes_of(&new_idx));
            self.set_raw(&full_k, &full_v)?;

            // remove old chain
            self.remove_raw(bytes_of(&ChainKey {
                idx,
                list_ph,
                namespace: CHAIN_NAMESPACE,
            }))?;

            new_idx += 1;
        }

        if list.tail_idx == new_idx {
            // list is now empty
            self.remove_raw(&list_key)?;
        } else {
            // update list head and tail, set holes=0
            self.set_raw(
                &list_key,
                bytes_of(&List {
                    head_idx: list.tail_idx,
                    tail_idx: new_idx,
                    num_items: new_idx - list.tail_idx,
                }),
            )?;
        }

        Ok(true)
    }

    /// Iterates over the elements of the list (identified by `list_key`) from the beginning (head)
    /// to the end (tail). Note that if items are removed at random locations in the list, the iterator
    /// will need to skip these holes. If you remove elements from the middle (not head/tail) of the list
    /// frequently, and wish to use iteration, consider compacting the list every so often using
    /// [Self::compact_list_if_needed]
    pub fn iter_list<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> ListIterator {
        self.owned_iter_list(list_key.as_ref().to_owned())
    }

    /// Owned version of [Self::iter_list]
    pub fn owned_iter_list(&self, list_key: Vec<u8>) -> ListIterator {
        let (list_ph, list_key) = self.make_list_key(list_key);
        ListIterator {
            store: &self,
            list_key,
            list_ph,
            range: None,
            fwd: true,
        }
    }

    /// Same as [Self::iter_list] but iterates from the end (tail) to the beginning (head)
    pub fn iter_list_backwards<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> ListIterator {
        self.owned_iter_list_backwards(list_key.as_ref().to_owned())
    }

    /// Owned version of [Self::iter_list_backwards]
    pub fn owned_iter_list_backwards(&self, list_key: Vec<u8>) -> ListIterator {
        let (list_ph, list_key) = self.make_list_key(list_key);
        ListIterator {
            store: &self,
            list_key,
            list_ph,
            range: None,
            fwd: false,
        }
    }

    /// Discards the given list, removing all elements it contains and dropping the list itself.
    /// This is more efficient than iteration + removal of each element.
    pub fn discard_list<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<bool> {
        self.owned_discard_list(list_key.as_ref().to_owned())
    }

    /// Owned version of [Self::discard_list]
    pub fn owned_discard_list(&self, list_key: Vec<u8>) -> Result<bool> {
        let (list_ph, list_key) = self.make_list_key(list_key);
        let _guard = self.lock_list(list_ph);

        let Some(list_bytes) = self.get_raw(&list_key)? else {
            return Ok(false);
        };
        let list = *from_bytes::<List>(&list_bytes);
        for idx in list.head_idx..list.tail_idx {
            let Some((_, full_key, _)) = self.get_from_list_at_index(list_ph, idx, false)? else {
                continue;
            };
            self.remove_raw(bytes_of(&ChainKey {
                list_ph,
                idx,
                namespace: CHAIN_NAMESPACE,
            }))?;
            self.remove_raw(&full_key)?;
        }
        self.remove_raw(&list_key)?;

        Ok(true)
    }

    /// Returns the first (head) element of the list
    pub fn peek_list_head<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_peek_list_head(list_key.as_ref().to_owned())
    }

    /// Owned version of [Self::peek_list_head]
    pub fn owned_peek_list_head(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        let Some(kv) = self.owned_iter_list(list_key).next() else {
            return Ok(None);
        };
        Ok(Some(kv?))
    }

    /// Returns the last (tail) element of the list
    pub fn peek_list_tail<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_peek_list_tail(list_key.as_ref().to_owned())
    }

    /// Owned version of [Self::peek_list_tail]
    pub fn owned_peek_list_tail(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        for kv in self.owned_iter_list_backwards(list_key) {
            return Ok(Some(kv?));
        }
        Ok(None)
    }

    /// Removes and returns the first (head) element of the list
    pub fn pop_list_head<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_pop_list_head(list_key.as_ref().to_owned())
    }

    fn _operate_on_list<T>(
        &self,
        list_key: Vec<u8>,
        default: T,
        func: impl FnOnce(PartedHash, Vec<u8>, List) -> Result<T>,
    ) -> Result<T> {
        let (list_ph, list_key) = self.make_list_key(list_key);
        let _guard = self.lock_list(list_ph);
        let Some(list_bytes) = self.get_raw(&list_key)? else {
            return Ok(default);
        };
        let list = *from_bytes::<List>(&list_bytes);
        func(list_ph, list_key, list)
    }

    fn _owned_pop_list(&self, list_key: Vec<u8>, fwd: bool) -> Result<Option<KVPair>> {
        self._operate_on_list(list_key, None, |list_ph, list_key, mut list| {
            let range = list.head_idx..list.tail_idx;

            let mut pop = |idx| -> Result<Option<KVPair>> {
                let Some((_, mut untrunc_k, mut untrunc_v)) =
                    self.get_from_list_at_index(list_ph, idx, false)?
                else {
                    return Ok(None);
                };

                if fwd {
                    list.head_idx = idx + 1;
                } else {
                    list.tail_idx = idx - 1;
                }
                list.num_items -= 1;
                if list.is_empty() {
                    self.remove_raw(&list_key)?;
                } else {
                    self.set_raw(&list_key, bytes_of(&list))?;
                }

                // remove chain
                self.remove_raw(bytes_of(&ChainKey {
                    list_ph,
                    idx,
                    namespace: CHAIN_NAMESPACE,
                }))?;

                // remove item
                self.remove_raw(&untrunc_k)?;

                untrunc_v.truncate(untrunc_v.len() - size_of::<u64>());
                untrunc_k.truncate(untrunc_k.len() - Self::LIST_KEY_SUFFIX_LEN);
                Ok(Some((untrunc_k, untrunc_v)))
            };

            if fwd {
                for idx in range {
                    if let Some(kv) = pop(idx)? {
                        return Ok(Some(kv));
                    }
                }
            } else {
                for idx in range.rev() {
                    if let Some(kv) = pop(idx)? {
                        return Ok(Some(kv));
                    }
                }
            }

            Ok(None)
        })
    }

    /// Owned version of [Self::peek_list_tail]
    pub fn owned_pop_list_head(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        self._owned_pop_list(list_key, true /* fwd */)
    }

    /// Removes and returns the last (tail) element of the list
    pub fn pop_list_tail<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_pop_list_tail(list_key.as_ref().to_owned())
    }

    /// Owned version of [Self::peek_list_tail]
    pub fn owned_pop_list_tail(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        self._owned_pop_list(list_key, false /* fwd */)
    }

    /// Returns the estimated list length
    pub fn list_len<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<usize> {
        self.owned_list_len(list_key.as_ref().to_owned())
    }
    pub fn owned_list_len(&self, list_key: Vec<u8>) -> Result<usize> {
        let (_, list_key) = self.make_list_key(list_key);

        let Some(list_bytes) = self.get_raw(&list_key)? else {
            return Ok(0);
        };

        Ok(from_bytes::<List>(&list_bytes).num_items as usize)
    }

    /// iterate over the given list and retain all elements for which the predicate returns `true`. In other
    /// words, drop all other elements. This operation is not crash safe, and holds the list locked during the
    /// whole iteration, so no other gets/sets/deletes can be done in by other threads on this list while
    /// iterating over it. Beware of deadlocks.
    ///
    /// This operation will also compact the list, basically popping all elements and re-pushing the retained
    /// ones at the end, so no holes will exist by the end.
    pub fn retain_in_list<B: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B,
        func: impl FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        self.owned_retain_in_list(list_key.as_ref().to_owned(), func)
    }

    /// owned version of [Self::retain_in_list]
    pub fn owned_retain_in_list(
        &self,
        list_key: Vec<u8>,
        mut func: impl FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        self._operate_on_list(list_key, (), |list_ph, list_key, mut list| {
            let range = list.head_idx..list.tail_idx;

            for idx in range {
                list.head_idx = idx + 1;
                let Some((item_ph, untrunc_k, mut untrunc_v)) =
                    self.get_from_list_at_index(list_ph, idx, false)?
                else {
                    continue;
                };

                untrunc_v.truncate(untrunc_v.len() - size_of::<u64>());
                let mut v = untrunc_v;
                let k = &untrunc_k[..untrunc_k.len() - Self::LIST_KEY_SUFFIX_LEN];

                // remove chain
                self.remove_raw(bytes_of(&ChainKey {
                    list_ph,
                    idx,
                    namespace: CHAIN_NAMESPACE,
                }))?;

                if func(k, &v)? {
                    let tail_idx = list.tail_idx;
                    list.tail_idx += 1;

                    // create chain
                    self.set_raw(
                        bytes_of(&ChainKey {
                            list_ph,
                            idx: tail_idx,
                            namespace: CHAIN_NAMESPACE,
                        }),
                        bytes_of(&item_ph),
                    )?;

                    // create new item
                    v.extend_from_slice(bytes_of(&tail_idx));
                    self.set_raw(&untrunc_k, &v)?;
                } else {
                    // drop from list
                    list.num_items -= 1;

                    // remove item
                    self.remove_raw(&untrunc_k)?;
                }
            }
            // defer updating the list to the very end to save on IOs
            if list.is_empty() {
                self.remove_raw(&list_key)?;
            } else {
                self.set_raw(&list_key, bytes_of(&list))?;
            }
            Ok(())
        })
    }
}
