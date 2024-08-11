use crate::{
    hashing::PartedHash,
    shard::{InsertMode, KVPair},
    store::{ITEM_NAMESPACE, LIST_NAMESPACE},
    GetOrCreateStatus, ReplaceStatus, Result, SetStatus, VickyError, VickyStore,
};

use anyhow::anyhow;
use bytemuck::{bytes_of, from_bytes, Pod, Zeroable};
use parking_lot::MutexGuard;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct LinkedList {
    tail: PartedHash,
    head: PartedHash,
}

#[derive(Debug, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct Chain {
    prev: PartedHash,
    next: PartedHash,
}

impl Chain {
    const INVALID: Self = Self {
        prev: PartedHash::INVALID,
        next: PartedHash::INVALID,
    };
}

const ITEM_SUFFIX_LEN: usize = size_of::<PartedHash>() + ITEM_NAMESPACE.len();

fn chain_of(buf: &[u8]) -> Chain {
    bytemuck::pod_read_unaligned(&buf[buf.len() - size_of::<Chain>()..])
}

pub struct LinkedListIterator<'a> {
    store: &'a VickyStore,
    list_key: Vec<u8>,
    list_ph: PartedHash,
    next_ph: Option<PartedHash>,
}

impl<'a> Iterator for LinkedListIterator<'a> {
    type Item = Result<Option<KVPair>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next_ph.is_none() {
            let buf = match self.store.get_raw(&self.list_key) {
                Ok(buf) => buf,
                Err(e) => return Some(Err(e)),
            };
            let Some(buf) = buf else {
                return None;
            };
            let list = *from_bytes::<LinkedList>(&buf);
            self.next_ph = Some(list.head);
        }
        let Some(curr) = self.next_ph else {
            return None;
        };
        if curr == PartedHash::INVALID {
            return None;
        }
        let kv = match self.store._list_get(self.list_ph, curr) {
            Err(e) => return Some(Err(e)),
            Ok(kv) => kv,
        };
        let Some((mut k, mut v)) = kv else {
            // this means the current element was removed by another thread, and that's okay
            // because we don't hold any locks during iteration. this is an early stop,
            // which means the reader might want to retry
            return Some(Ok(None));
        };
        k.truncate(k.len() - ITEM_SUFFIX_LEN);
        let chain = chain_of(&v);
        self.next_ph = Some(chain.next);
        v.truncate(v.len() - size_of::<Chain>());

        Some(Ok(Some((k, v))))
    }
}

// it doesn't really make sense to implement DoubleEndedIterator here, because we'd have to maintain both
// pointers and the protocol says iteration ends when they meet in the middle
pub struct RevLinkedListIterator<'a> {
    store: &'a VickyStore,
    list_key: Vec<u8>,
    list_ph: PartedHash,
    next_ph: Option<PartedHash>,
}

impl<'a> Iterator for RevLinkedListIterator<'a> {
    type Item = Result<Option<KVPair>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next_ph.is_none() {
            let buf = match self.store.get_raw(&self.list_key) {
                Ok(buf) => buf,
                Err(e) => return Some(Err(e)),
            };
            let Some(buf) = buf else {
                return None;
            };
            let list = *from_bytes::<LinkedList>(&buf);
            self.next_ph = Some(list.tail);
        }
        let Some(curr) = self.next_ph else {
            return None;
        };
        if curr == PartedHash::INVALID {
            return None;
        }
        let kv = match self.store._list_get(self.list_ph, curr) {
            Err(e) => return Some(Err(e)),
            Ok(kv) => kv,
        };
        let Some((mut k, mut v)) = kv else {
            // this means the current element was removed by another thread, and that's okay
            // because we don't hold any locks during iteration. this is an early stop,
            // which means the reader might want to retry
            return Some(Ok(None));
        };
        k.truncate(k.len() - ITEM_SUFFIX_LEN);
        let chain = chain_of(&v);
        self.next_ph = Some(chain.prev);
        v.truncate(v.len() - size_of::<Chain>());

        Some(Ok(Some((k, v))))
    }
}

macro_rules! corrupted_list {
    ($($arg:tt)*) => {
        return Err(anyhow!(VickyError::CorruptedLinkedList(format!($($arg)*))));
    };
}

macro_rules! corrupted_if {
    ($e1: expr, $e2: expr, $($arg:tt)*) => {
        if ($e1 == $e2) {
            let tmp = format!($($arg)*);
            let full = format!("{tmp} ({:?} == {:?})", $e1, $e2);
            return Err(anyhow!(VickyError::CorruptedLinkedList(full)));
        }
    };
}

macro_rules! corrupted_unless {
    ($e1: expr, $e2: expr, $($arg:tt)*) => {
        if ($e1 != $e2) {
            let tmp = format!($($arg)*);
            let full = format!("{tmp} ({:?} != {:?})", $e1, $e2);
            return Err(anyhow!(VickyError::CorruptedLinkedList(full)));
        }
    };
}

impl VickyStore {
    fn make_list_key(&self, mut list_key: Vec<u8>) -> (PartedHash, Vec<u8>) {
        list_key.extend_from_slice(LIST_NAMESPACE);
        (PartedHash::new(&self.config.hash_seed, &list_key), list_key)
    }

    fn make_item_key(&self, list_ph: PartedHash, mut item_key: Vec<u8>) -> (PartedHash, Vec<u8>) {
        item_key.extend_from_slice(bytes_of(&list_ph));
        item_key.extend_from_slice(ITEM_NAMESPACE);
        (PartedHash::new(&self.config.hash_seed, &item_key), item_key)
    }

    fn _list_lock(&self, list_ph: PartedHash) -> MutexGuard<()> {
        self.keyed_locks[(list_ph.signature() & self.keyed_locks_mask) as usize].lock()
    }

    fn _list_get(&self, list_ph: PartedHash, item_ph: PartedHash) -> Result<Option<KVPair>> {
        let mut suffix = [0u8; ITEM_SUFFIX_LEN];
        suffix[0..PartedHash::LEN].copy_from_slice(bytes_of(&list_ph));
        suffix[PartedHash::LEN..].copy_from_slice(ITEM_NAMESPACE);

        for res in self.get_by_hash(item_ph)? {
            let (k, v) = res?;
            if k.ends_with(&suffix) {
                return Ok(Some((k, v)));
            }
        }
        Ok(None)
    }

    fn find_true_tail(
        &self,
        list_ph: PartedHash,
        tail: PartedHash,
    ) -> Result<(PartedHash, Vec<u8>, Vec<u8>)> {
        let mut curr = tail;
        let mut prev = None;
        loop {
            if let Some((k, v)) = self._list_get(list_ph, curr)? {
                let chain = chain_of(&v);
                if chain.next == PartedHash::INVALID {
                    // curr is the true tail
                    return Ok((curr, k, v));
                }
                prev = Some((curr, k, v));
                curr = chain.next;
            } else if let Some(prev) = prev {
                // prev is the true tail
                assert_ne!(curr, tail);
                return Ok(prev);
            } else {
                // if prev=None, it means we weren't able to find list.tail. this should never happen
                assert_eq!(curr, tail);
                corrupted_list!("tail {:?} does not exist", tail);
            }
        }
    }

    fn _insert_to_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        mut val: Vec<u8>,
        mode: InsertMode,
    ) -> Result<GetOrCreateStatus> {
        let (list_ph, list_key) = self.make_list_key(list_key);
        let (item_ph, item_key) = self.make_item_key(list_ph, item_key);

        let _guard = self._list_lock(list_ph);

        // if the item already exists, it means it belongs to this list. we just need to update the value and
        // keep the existing chain part
        if let Some(mut old_val) = self.get_raw(&item_key)? {
            if matches!(mode, InsertMode::GetOrCreate) {
                // don't replace the existing value
                old_val.truncate(old_val.len() - size_of::<Chain>());
                return Ok(GetOrCreateStatus::ExistingValue(old_val));
            }

            val.extend_from_slice(&old_val[old_val.len() - size_of::<Chain>()..]);
            match self.replace_raw(&item_key, &val)? {
                ReplaceStatus::DoesNotExist => {
                    corrupted_list!("failed replacing existing item");
                }
                ReplaceStatus::PrevValue(mut v) => {
                    v.truncate(v.len() - size_of::<Chain>());
                    return Ok(GetOrCreateStatus::ExistingValue(v));
                }
            }
        }

        if matches!(mode, InsertMode::Replace) {
            // not allowed to create
            return Ok(GetOrCreateStatus::CreatedNew(val));
        }

        // item does not exist, and the list itself might also not exist. get or create the list
        let curr_list = LinkedList {
            tail: item_ph,
            head: item_ph,
        };

        let curr_list = *from_bytes::<LinkedList>(
            &self
                .get_or_create_raw(&list_key, bytes_of(&curr_list).to_vec())?
                .value(),
        );

        // we have the list. if the list points to this item, it means we've just created it
        // this first time should have prev=INVALID and next=INVALID
        if curr_list.head == item_ph {
            corrupted_unless!(curr_list.tail, item_ph, "head != tail");
            val.extend_from_slice(bytes_of(&Chain::INVALID));
            if !self.set_raw(&item_key, &val)?.was_created() {
                corrupted_list!("expected to create {item_key:?}");
            }
            val.truncate(val.len() - size_of::<Chain>());
            return Ok(GetOrCreateStatus::CreatedNew(val));
        }

        // the list already exists. start at list.tail and find the true tail (it's possible list.tail
        // isn't up to date because of crashes)
        let (tail_ph, tail_k, tail_v) = self.find_true_tail(list_ph, curr_list.tail)?;

        // modify the last item to point to the new item. if we crash after this, everything is okay because
        // find_true_tail will stop at this item
        let mut tail_chain = chain_of(&tail_v);
        tail_chain.next = item_ph;

        if !self
            .modify_inplace_raw(
                &tail_k,
                bytes_of(&tail_chain),
                tail_v.len() - size_of::<Chain>(),
                Some(&tail_v[tail_v.len() - size_of::<Chain>()..]),
            )?
            .was_replaced()
        {
            corrupted_list!(
                "failed to update previous element {tail_v:?} to point to this one {item_key:?}"
            );
        }

        // now add item, with prev pointing to the old tail. if we crash after this, find_true_tail
        // will return the newly-added item as the tail.
        let this_chain = Chain {
            next: PartedHash::INVALID,
            prev: tail_ph,
        };
        val.extend_from_slice(bytes_of(&this_chain));

        if self.set_raw(&item_key, &val)?.was_replaced() {
            corrupted_list!("tail element {item_key:?} already exists");
        }

        // now update the list to point to the new tail. if we crash before it's committed, all's good
        let new_list = LinkedList {
            head: curr_list.head,
            tail: item_ph,
        };

        if !self
            .modify_inplace_raw(
                &list_key,
                bytes_of(&new_list),
                0,
                Some(bytes_of(&curr_list)),
            )?
            .was_replaced()
        {
            corrupted_list!("failed to update list tail to point to {item_key:?}");
        }
        val.truncate(val.len() - size_of::<Chain>());
        Ok(GetOrCreateStatus::CreatedNew(val))
    }

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
        )
    }

    pub fn owned_set_in_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        val: Vec<u8>,
    ) -> Result<SetStatus> {
        match self._insert_to_list(list_key, item_key, val, InsertMode::Set)? {
            GetOrCreateStatus::CreatedNew(_) => Ok(SetStatus::CreatedNew),
            GetOrCreateStatus::ExistingValue(v) => Ok(SetStatus::PrevValue(v)),
        }
    }

    pub fn replace_in_list<
        B1: AsRef<[u8]> + ?Sized,
        B2: AsRef<[u8]> + ?Sized,
        B3: AsRef<[u8]> + ?Sized,
    >(
        &self,
        list_key: &B1,
        item_key: &B2,
        val: &B3,
    ) -> Result<ReplaceStatus> {
        self.owned_replace_in_list(
            list_key.as_ref().to_owned(),
            item_key.as_ref().to_owned(),
            val.as_ref().to_owned(),
        )
    }

    pub fn owned_replace_in_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        val: Vec<u8>,
    ) -> Result<ReplaceStatus> {
        match self._insert_to_list(list_key, item_key, val, InsertMode::Replace)? {
            GetOrCreateStatus::CreatedNew(_) => Ok(ReplaceStatus::DoesNotExist),
            GetOrCreateStatus::ExistingValue(v) => Ok(ReplaceStatus::PrevValue(v)),
        }
    }

    pub fn get_or_create_in_list<
        B1: AsRef<[u8]> + ?Sized,
        B2: AsRef<[u8]> + ?Sized,
        B3: AsRef<[u8]> + ?Sized,
    >(
        &self,
        list_key: &B1,
        item_key: &B2,
        val: &B3,
    ) -> Result<GetOrCreateStatus> {
        self.owned_get_or_create_in_list(
            list_key.as_ref().to_owned(),
            item_key.as_ref().to_owned(),
            val.as_ref().to_owned(),
        )
    }

    pub fn owned_get_or_create_in_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
        val: Vec<u8>,
    ) -> Result<GetOrCreateStatus> {
        self._insert_to_list(list_key, item_key, val, InsertMode::GetOrCreate)
    }

    pub fn get_from_list<B1: AsRef<[u8]> + ?Sized, B2: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B1,
        item_key: &B2,
    ) -> Result<Option<Vec<u8>>> {
        self.owned_get_from_list(list_key.as_ref().to_owned(), item_key.as_ref().to_owned())
    }

    pub fn owned_get_from_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let (list_ph, _) = self.make_list_key(list_key);
        let (_, item_key) = self.make_item_key(list_ph, item_key);
        let Some(mut v) = self.get_raw(&item_key)? else {
            return Ok(None);
        };
        v.truncate(v.len() - size_of::<Chain>());
        Ok(Some(v))
    }

    fn _remove_from_list_single(
        &self,
        _chain: Chain,
        list_key: Vec<u8>,
        item_key: &[u8],
    ) -> Result<()> {
        // this is the only element - remove it and the list itself
        // corrupted_unless!(
        //     chain.next,
        //     PartedHash::INVALID,
        //     "chain.next must be invalid"
        // );
        // corrupted_unless!(
        //     chain.prev,
        //     PartedHash::INVALID,
        //     "chain.prev must be invalid"
        // );

        self.remove_raw(&list_key)?;
        self.remove_raw(item_key)?;
        Ok(())
    }

    fn _remove_from_list_head(
        &self,
        list_buf: Vec<u8>,
        mut list: LinkedList,
        chain: Chain,
        list_ph: PartedHash,
        list_key: Vec<u8>,
        item_key: &[u8],
    ) -> Result<()> {
        // corrupted_unless!(
        //     chain.prev,
        //     PartedHash::INVALID,
        //     "first element must not have a valid prev {item_key:?}"
        // );
        corrupted_if!(
            chain.next,
            PartedHash::INVALID,
            "first element must have a valid next {item_key:?}"
        );

        // we surely have a next element
        let Some((next_k, next_v)) = self._list_get(list_ph, chain.next)? else {
            corrupted_list!("failed getting next of {item_key:?}");
        };

        // update list.head from this to this.next. if we crash afterwards, the list will start
        // at the expected place. XXX: head.prev will not be INVALID, which might break asserts.
        // we will need to remove the asserts, or add find_true_head
        list.head = chain.next;
        if !self
            .modify_inplace_raw(&list_key, bytes_of(&list), 0, Some(&list_buf))?
            .was_replaced()
        {
            corrupted_list!("failed updating list head to point to next");
        }

        // set the new head's prev link to INVALID. if we crash afterwards, everything is good.
        let mut next_chain = chain_of(&next_v);
        next_chain.prev = PartedHash::INVALID;
        if !self
            .modify_inplace_raw(
                &next_k,
                bytes_of(&next_chain),
                next_v.len() - size_of::<Chain>(),
                Some(&next_v[next_v.len() - size_of::<Chain>()..]),
            )?
            .was_replaced()
        {
            corrupted_list!("failed updating prev=INVALID on the now-first element");
        }

        // finally remove the item, sealing the deal
        self.remove_raw(&item_key)?;
        Ok(())
    }

    fn _remove_from_list_tail(
        &self,
        list_buf: Vec<u8>,
        mut list: LinkedList,
        chain: Chain,
        list_ph: PartedHash,
        list_key: Vec<u8>,
        item_key: &[u8],
    ) -> Result<()> {
        // corrupted_unless!(
        //     chain.next,
        //     PartedHash::INVALID,
        //     "last element must not have a valid next"
        // );
        corrupted_if!(
            chain.prev,
            PartedHash::INVALID,
            "last element must have a valid prev"
        );

        let Some((prev_k, prev_v)) = self._list_get(list_ph, chain.prev)? else {
            corrupted_list!("missing prev element {item_key:?}");
        };

        // point list.tail to the prev item. if we crash afterwards, the removed tail is still considered
        // part of the list (find_true_tai will find it)
        list.tail = chain.prev;
        if !self
            .modify_inplace_raw(&list_key, bytes_of(&list), 0, Some(&list_buf))?
            .was_replaced()
        {
            corrupted_list!("failed updating list tail to point to prev");
        }

        // update the new tail's next to INVALID. if we crash afterwards, the removed tail is no longer
        // considered part of the list
        let mut prev_chain = chain_of(&prev_v);
        prev_chain.next = PartedHash::INVALID;
        self.modify_inplace_raw(
            &prev_k,
            bytes_of(&prev_chain),
            prev_v.len() - size_of::<Chain>(),
            Some(&prev_v[prev_v.len() - size_of::<Chain>()..]),
        )?;

        // finally remove the item, sealing the deal
        self.remove_raw(&item_key)?;
        Ok(())
    }

    fn _remove_from_list_middle(
        &self,
        chain: Chain,
        list_ph: PartedHash,
        item_ph: PartedHash,
        item_key: Vec<u8>,
    ) -> Result<()> {
        // this is a "middle" item, it has a prev one and a next one. set prev.next = this.next,
        // set next.prev = prev, update list (for `len`)
        corrupted_if!(
            chain.prev,
            PartedHash::INVALID,
            "a middle element must have a valid prev"
        );
        corrupted_if!(
            chain.next,
            PartedHash::INVALID,
            "a middle element must have a valid next"
        );

        let Some((prev_k, prev_v)) = self._list_get(list_ph, chain.prev)? else {
            corrupted_list!("missing prev element of {item_key:?}");
        };
        let Some((next_k, next_v)) = self._list_get(list_ph, chain.next)? else {
            corrupted_list!("missing next element of {item_key:?}");
        };

        // disconnect the item from its prev. if we crash afterwards, the item is still considered part
        // of the list, but will no longer appear in iterations.
        // note: we only do that if the previous item thinks that we're its next item, otherwise it means
        // we crashed in the middle of such an operation before
        let mut prev_chain = chain_of(&prev_v);
        if prev_chain.next == item_ph {
            prev_chain.next = chain.next;
            if !self
                .modify_inplace_raw(
                    &prev_k,
                    bytes_of(&prev_chain),
                    prev_v.len() - size_of::<Chain>(),
                    Some(&prev_v[prev_v.len() - size_of::<Chain>()..]),
                )?
                .was_replaced()
            {
                corrupted_list!("failed updating prev.next on {prev_k:?}");
            }
        }

        // disconnect the item from its next. if we crash afterwards, the item is truly no longer linked to the
        // list, so everything's good
        let mut next_chain = chain_of(&next_v);
        if next_chain.prev == item_ph {
            next_chain.prev = chain.prev;
            if !self
                .modify_inplace_raw(
                    &next_k,
                    bytes_of(&next_chain),
                    next_v.len() - size_of::<Chain>(),
                    Some(&next_v[next_v.len() - size_of::<Chain>()..]),
                )?
                .was_replaced()
            {
                corrupted_list!("failed updating next.prev on {next_k:?}");
            }
        }

        // now it's safe to remove the item
        self.remove_raw(&item_key)?;
        Ok(())
    }

    pub fn remove_from_list<B1: AsRef<[u8]> + ?Sized, B2: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B1,
        item_key: &B2,
    ) -> Result<Option<Vec<u8>>> {
        self.owned_remove_from_list(list_key.as_ref().to_owned(), item_key.as_ref().to_owned())
    }

    pub fn owned_remove_from_list(
        &self,
        list_key: Vec<u8>,
        item_key: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let (list_ph, list_key) = self.make_list_key(list_key);
        let (item_ph, item_key) = self.make_item_key(list_ph, item_key);

        let _guard = self._list_lock(list_ph);

        // if the item does not exist -- all's good
        let Some(mut v) = self.get_raw(&item_key)? else {
            return Ok(None);
        };

        let chain = chain_of(&v);
        v.truncate(v.len() - size_of::<Chain>());

        // fetch the list
        let Some(list_buf) = self.get_raw(&list_key)? else {
            // if it does not exist, it means we've crashed right between removing the list and removing
            // the only item it held - proceed to removing this item
            self.remove_raw(&item_key)?;
            return Ok(Some(v));
        };

        let list = *from_bytes::<LinkedList>(&list_buf);

        if list.tail == item_ph && list.head == item_ph {
            self._remove_from_list_single(chain, list_key, &item_key)?
        } else if list.head == item_ph || chain.prev == PartedHash::INVALID {
            self._remove_from_list_head(list_buf, list, chain, list_ph, list_key, &item_key)?
        } else if list.tail == item_ph || chain.next == PartedHash::INVALID {
            self._remove_from_list_tail(list_buf, list, chain, list_ph, list_key, &item_key)?
        } else {
            self._remove_from_list_middle(chain, list_ph, item_ph, item_key)?
        };
        Ok(Some(v))
    }

    pub fn iter_list<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> LinkedListIterator {
        self.owned_iter_list(list_key.as_ref().to_owned())
    }
    pub fn owned_iter_list(&self, list_key: Vec<u8>) -> LinkedListIterator {
        let (list_ph, list_key) = self.make_list_key(list_key);
        LinkedListIterator {
            store: &self,
            list_key,
            list_ph,
            next_ph: None,
        }
    }

    pub fn iter_list_backwards<B: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B,
    ) -> RevLinkedListIterator {
        self.owned_iter_list_backwards(list_key.as_ref().to_owned())
    }

    pub fn owned_iter_list_backwards(&self, list_key: Vec<u8>) -> RevLinkedListIterator {
        let (list_ph, list_key) = self.make_list_key(list_key);
        RevLinkedListIterator {
            store: &self,
            list_key,
            list_ph,
            next_ph: None,
        }
    }

    /// Discards the given list (removes all elements). This also works for corrupt lists, in case they
    /// need to be dropped.
    pub fn discard_list<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<()> {
        self.owned_discard_list(list_key.as_ref().to_owned())
    }

    pub fn owned_discard_list(&self, list_key: Vec<u8>) -> Result<()> {
        let (list_ph, list_key) = self.make_list_key(list_key);

        let _guard = self._list_lock(list_ph);

        let Some(list_buf) = self.remove_raw(&list_key)? else {
            return Ok(());
        };
        let list = *from_bytes::<LinkedList>(&list_buf);
        let mut curr = list.head;

        while curr != PartedHash::INVALID {
            let Some((k, v)) = self._list_get(list_ph, curr)? else {
                break;
            };

            curr = chain_of(&v).next;
            self.remove_raw(&k)?;
        }

        Ok(())
    }

    pub fn peek_list_head<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_peek_list_head(list_key.as_ref().to_owned())
    }

    // may have suprious false positives/false negatives if another thread pops
    pub fn owned_peek_list_head(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        self.owned_iter_list(list_key).next().unwrap_or(Ok(None))
    }

    pub fn peek_list_htail<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_peek_list_tail(list_key.as_ref().to_owned())
    }

    // may have suprious false positives/false negatives if another thread pops
    pub fn owned_peek_list_tail(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        self.owned_iter_list_backwards(list_key)
            .next()
            .unwrap_or(Ok(None))
    }

    pub fn pop_list_head<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_pop_list_head(list_key.as_ref().to_owned())
    }

    pub fn owned_pop_list_head(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        let (list_ph, list_key) = self.make_list_key(list_key);

        let _guard = self._list_lock(list_ph);

        let Some(list_buf) = self.get_raw(&list_key)? else {
            return Ok(None);
        };
        let list = *from_bytes::<LinkedList>(&list_buf);
        let item_ph = list.head;
        let Some((item_key, mut item_val)) = self._list_get(list_ph, item_ph)? else {
            return Ok(None);
        };

        let chain = chain_of(&item_val);
        item_val.truncate(item_val.len() - size_of::<Chain>());

        if list.tail == item_ph {
            self._remove_from_list_single(chain, list_key, &item_key)?;
        } else {
            self._remove_from_list_head(list_buf, list, chain, list_ph, list_key, &item_key)?;
        }
        Ok(Some((item_key, item_val)))
    }

    pub fn pop_list_tail<B: AsRef<[u8]> + ?Sized>(&self, list_key: &B) -> Result<Option<KVPair>> {
        self.owned_pop_list_tail(list_key.as_ref().to_owned())
    }

    pub fn owned_pop_list_tail(&self, list_key: Vec<u8>) -> Result<Option<KVPair>> {
        let (list_ph, list_key) = self.make_list_key(list_key);

        let _guard = self._list_lock(list_ph);

        let Some(list_buf) = self.get_raw(&list_key)? else {
            return Ok(None);
        };
        let list = *from_bytes::<LinkedList>(&list_buf);
        let item_ph = list.tail;
        let Some((item_key, mut item_val)) = self._list_get(list_ph, item_ph)? else {
            return Ok(None);
        };

        let chain = chain_of(&item_val);
        item_val.truncate(item_val.len() - size_of::<Chain>());

        if list.head == item_ph {
            self._remove_from_list_single(chain, list_key, &item_key)?;
        } else {
            self._remove_from_list_tail(list_buf, list, chain, list_ph, list_key, &item_key)?;
        }
        Ok(Some((item_key, item_val)))
    }

    pub fn push_to_list<B1: AsRef<[u8]> + ?Sized, B2: AsRef<[u8]> + ?Sized>(
        &self,
        list_key: &B1,
        val: &B2,
    ) -> Result<Uuid> {
        self.owned_push_to_list(list_key.as_ref().to_owned(), val.as_ref().to_owned())
    }

    pub fn owned_push_to_list(&self, list_key: Vec<u8>, val: Vec<u8>) -> Result<Uuid> {
        let uuid = Uuid::new_v4();
        let status = self._insert_to_list(
            list_key,
            uuid.as_bytes().to_vec(),
            val,
            InsertMode::GetOrCreate,
        )?;
        if !status.was_created() {
            corrupted_list!("uuid collision {uuid}");
        }
        Ok(uuid)
    }
}
