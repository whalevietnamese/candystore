use anyhow::anyhow;
use std::{borrow::Borrow, marker::PhantomData, sync::Arc};
use uuid::Uuid;

use crate::{
    insertion::{ReplaceStatus, SetStatus},
    store::TYPED_NAMESPACE,
    ModifyStatus, VickyStore,
};

use crate::Result;
use databuf::{config::num::LE, DecodeOwned, Encode};

pub trait VickyTypedKey: Encode + DecodeOwned {
    /// a random number that remains consistent (unlike [std::any::TypeId]), so that `MyPair(u32, u32)`
    /// is different from `YourPair(u32, u32)`
    const TYPE_ID: u32;
}

macro_rules! typed_builtin {
    ($t:ty, $v:literal) => {
        impl VickyTypedKey for $t {
            const TYPE_ID: u32 = $v;
        }
    };
}

typed_builtin!(u8, 1);
typed_builtin!(u16, 2);
typed_builtin!(u32, 3);
typed_builtin!(u64, 4);
typed_builtin!(u128, 5);
typed_builtin!(i8, 6);
typed_builtin!(i16, 7);
typed_builtin!(i32, 8);
typed_builtin!(i64, 9);
typed_builtin!(i128, 10);
typed_builtin!(bool, 11);
typed_builtin!(usize, 12);
typed_builtin!(isize, 13);
typed_builtin!(char, 14);
typed_builtin!(String, 15);
typed_builtin!(Vec<u8>, 16);

fn from_bytes<T: DecodeOwned>(bytes: &[u8]) -> Result<T> {
    T::from_bytes::<LE>(bytes).map_err(|e| anyhow!(e))
}

/// Typed stores are wrappers around an underlying [VickyStore], that serialize keys and values (using [databuf]).
/// These are but thin wrappers, and multiple such wrappers can exist over the same store.
///
/// The keys and values must support [Encode] and [DecodeOwned], with the addition that keys also provide
/// a `TYPE_ID` const, via the [VickyTypedKey] trait.
///
/// Notes:
/// * All APIs take keys and values by-ref, because they will serialize them, so taking owned values doesn't
///   make sense
/// * [VickyStore::iter] will skip typed items, since it's meaningless to interpret them without the wrapper
#[derive(Clone)]
pub struct VickyTypedStore<K, V> {
    store: Arc<VickyStore>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> VickyTypedStore<K, V>
where
    K: VickyTypedKey,
    V: Encode + DecodeOwned,
{
    /// Constructs a typed wrapper over a VickyStore
    pub fn new(store: Arc<VickyStore>) -> Self {
        Self {
            store,
            _phantom: Default::default(),
        }
    }

    fn make_key<Q: ?Sized + Encode>(key: &Q) -> Vec<u8>
    where
        K: Borrow<Q>,
    {
        let mut kbytes = key.to_bytes::<LE>();
        kbytes.extend_from_slice(&K::TYPE_ID.to_le_bytes());
        kbytes.extend_from_slice(TYPED_NAMESPACE);
        kbytes
    }

    /// Same as [VickyStore::contains] but serializes the key
    pub fn contains<Q: ?Sized + Encode>(&self, key: &Q) -> Result<bool>
    where
        K: Borrow<Q>,
    {
        Ok(self.store.get_raw(&Self::make_key(key))?.is_some())
    }

    /// Same as [VickyStore::get] but serializes the key and deserializes the value
    pub fn get<Q: ?Sized + Encode>(&self, key: &Q) -> Result<Option<V>>
    where
        K: Borrow<Q>,
    {
        let kbytes = Self::make_key(key);
        if let Some(vbytes) = self.store.get_raw(&kbytes)? {
            Ok(Some(from_bytes::<V>(&vbytes)?))
        } else {
            Ok(None)
        }
    }

    /// Same as [VickyStore::replace] but serializes the key and the value
    pub fn replace<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        key: &Q1,
        val: &Q2,
    ) -> Result<Option<V>>
    where
        K: Borrow<Q1>,
        V: Borrow<Q2>,
    {
        let kbytes = Self::make_key(key);
        let vbytes = val.to_bytes::<LE>();
        match self.store.replace_raw(&kbytes, &vbytes)? {
            ReplaceStatus::DoesNotExist => Ok(None),
            ReplaceStatus::PrevValue(v) => Ok(Some(from_bytes::<V>(&v)?)),
        }
    }

    /// Same as [VickyStore::replace_inplace] but serializes the key and the value.
    /// Note: not crash safe!
    pub fn replace_inplace<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        key: &Q1,
        val: &Q2,
    ) -> Result<Option<V>>
    where
        K: Borrow<Q1>,
        V: Borrow<Q2>,
    {
        let kbytes = Self::make_key(key);
        let vbytes = val.to_bytes::<LE>();
        match self.store.replace_inplace_raw(&kbytes, &vbytes)? {
            ModifyStatus::DoesNotExist => Ok(None),
            ModifyStatus::PrevValue(v) => Ok(Some(from_bytes::<V>(&v)?)),
            ModifyStatus::ValueMismatch(_) => unreachable!(),
            ModifyStatus::ValueTooLong(_, _, _) => Ok(None),
            ModifyStatus::WrongLength(_, _) => Ok(None),
        }
    }

    /// Same as [VickyStore::set] but serializes the key and the value.
    pub fn set<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        key: &Q1,
        val: &Q2,
    ) -> Result<Option<V>>
    where
        K: Borrow<Q1>,
        V: Borrow<Q2>,
    {
        let kbytes = Self::make_key(key);
        let vbytes = val.to_bytes::<LE>();
        match self.store.set_raw(&kbytes, &vbytes)? {
            SetStatus::CreatedNew => Ok(None),
            SetStatus::PrevValue(v) => Ok(Some(from_bytes::<V>(&v)?)),
        }
    }

    /// Same as [VickyStore::get_or_create] but serializes the key and the default value
    pub fn get_or_create<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        key: &Q1,
        default_val: &Q2,
    ) -> Result<V>
    where
        K: Borrow<Q1>,
        V: Borrow<Q2>,
    {
        let kbytes = Self::make_key(key);
        Ok(from_bytes::<V>(
            &self
                .store
                .get_or_create_raw(&kbytes, default_val.to_bytes::<LE>())?
                .value(),
        )?)
    }

    /// Same as [VickyStore::remove] but serializes the key
    pub fn remove<Q: ?Sized + Encode>(&self, k: &Q) -> Result<Option<V>>
    where
        K: Borrow<Q>,
    {
        let kbytes = Self::make_key(k);
        if let Some(vbytes) = self.store.remove_raw(&kbytes)? {
            Ok(Some(from_bytes::<V>(&vbytes)?))
        } else {
            Ok(None)
        }
    }
}

/// A wrapper around [VickyStore] that exposes the linked-list API in a typed manner. See [VickyTypedStore] for more
/// info
#[derive(Clone)]
pub struct VickyTypedList<L, K, V> {
    store: Arc<VickyStore>,
    _phantom: PhantomData<(L, K, V)>,
}

impl<L, K, V> VickyTypedList<L, K, V>
where
    L: VickyTypedKey,
    K: Encode + DecodeOwned,
    V: Encode + DecodeOwned,
{
    /// Constructs a [VickyTypedList] over an existing [VickyStore]
    pub fn new(store: Arc<VickyStore>) -> Self {
        Self {
            store,
            _phantom: PhantomData,
        }
    }

    fn make_list_key<Q: ?Sized + Encode>(list_key: &Q) -> Vec<u8>
    where
        L: Borrow<Q>,
    {
        let mut kbytes = list_key.to_bytes::<LE>();
        kbytes.extend_from_slice(&L::TYPE_ID.to_le_bytes());
        kbytes
    }

    /// Tests if the given typed `item_key` exists in this list (identified by `list_key`)
    pub fn contains<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
    ) -> Result<bool>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
    {
        let list_key = Self::make_list_key(list_key);
        let item_key = item_key.to_bytes::<LE>();
        Ok(self
            .store
            .owned_get_from_list(list_key, item_key)?
            .is_some())
    }

    /// Same as [VickyStore::get_from_list], but `list_key` and `item_key` are typed
    pub fn get<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
    ) -> Result<Option<V>>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
    {
        let list_key = Self::make_list_key(list_key);
        let item_key = item_key.to_bytes::<LE>();
        if let Some(vbytes) = self.store.owned_get_from_list(list_key, item_key)? {
            Ok(Some(from_bytes::<V>(&vbytes)?))
        } else {
            Ok(None)
        }
    }

    fn _set<Q1: ?Sized + Encode, Q2: ?Sized + Encode, Q3: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
        val: &Q3,
        promote: bool,
    ) -> Result<Option<V>>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
        V: Borrow<Q3>,
    {
        let list_key = Self::make_list_key(list_key);
        let item_key = item_key.to_bytes::<LE>();
        let val = val.to_bytes::<LE>();
        match self
            .store
            .owned_set_in_list(list_key, item_key, val, promote)?
        {
            SetStatus::CreatedNew => Ok(None),
            SetStatus::PrevValue(v) => Ok(Some(from_bytes::<V>(&v)?)),
        }
    }

    /// Same as [VickyStore::set_in_list], but `list_key`, `item_key` and `val` are typed
    pub fn set<Q1: ?Sized + Encode, Q2: ?Sized + Encode, Q3: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
        val: &Q3,
    ) -> Result<Option<V>>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
        V: Borrow<Q3>,
    {
        self._set(list_key, item_key, val, false)
    }

    /// Same as [VickyStore::set_in_list_promoting], but `list_key`, `item_key` and `val` are typed
    pub fn set_promoting<Q1: ?Sized + Encode, Q2: ?Sized + Encode, Q3: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
        val: &Q3,
    ) -> Result<Option<V>>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
        V: Borrow<Q3>,
    {
        self._set(list_key, item_key, val, true)
    }

    /// Same as [VickyStore::get_or_create_in_list], but `list_key`, `item_key` and `default_val` are typed
    pub fn get_or_create<Q1: ?Sized + Encode, Q2: ?Sized + Encode, Q3: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
        default_val: &Q3,
    ) -> Result<V>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
    {
        let list_key = Self::make_list_key(list_key);
        let item_key = item_key.to_bytes::<LE>();
        let default_val = default_val.to_bytes::<LE>();
        let vbytes = self
            .store
            .owned_get_or_create_in_list(list_key, item_key, default_val)?
            .value();
        from_bytes::<V>(&vbytes)
    }

    /// Same as [VickyStore::replace_in_list], but `list_key`, `item_key` and `val` are typed
    pub fn replace<Q1: ?Sized + Encode, Q2: ?Sized + Encode, Q3: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
        val: &Q3,
    ) -> Result<Option<V>>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
        V: Borrow<Q3>,
    {
        let list_key = Self::make_list_key(list_key);
        let item_key = item_key.to_bytes::<LE>();
        let val = val.to_bytes::<LE>();
        match self.store.owned_replace_in_list(list_key, item_key, val)? {
            ReplaceStatus::DoesNotExist => Ok(None),
            ReplaceStatus::PrevValue(v) => Ok(Some(from_bytes::<V>(&v)?)),
        }
    }

    /// Same as [VickyStore::remove_from_list], but `list_key` and `item_key`  are typed
    pub fn remove<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        item_key: &Q2,
    ) -> Result<Option<V>>
    where
        L: Borrow<Q1>,
        K: Borrow<Q2>,
    {
        let list_key = Self::make_list_key(list_key);
        let item_key = item_key.to_bytes::<LE>();
        if let Some(vbytes) = self.store.owned_remove_from_list(list_key, item_key)? {
            Ok(Some(from_bytes::<V>(&vbytes)?))
        } else {
            Ok(None)
        }
    }

    /// Same as [VickyStore::iter_list], but `list_key` is typed
    pub fn iter<'a, Q: ?Sized + Encode>(
        &'a self,
        list_key: &Q,
    ) -> impl Iterator<Item = Result<Option<(K, V)>>> + 'a
    where
        L: Borrow<Q>,
    {
        let list_key = Self::make_list_key(list_key);
        self.store.owned_iter_list(list_key).map(|res| match res {
            Err(e) => Err(e),
            Ok(None) => Ok(None),
            Ok(Some((k, v))) => {
                let key = from_bytes::<K>(&k)?;
                let val = from_bytes::<V>(&v)?;
                Ok(Some((key, val)))
            }
        })
    }

    /// Same as [VickyStore::discard_list], but `list_key` is typed
    pub fn discard<Q: ?Sized + Encode>(&self, list_key: &Q) -> Result<()>
    where
        L: Borrow<Q>,
    {
        let list_key = Self::make_list_key(list_key);
        self.store.owned_discard_list(list_key)
    }

    /// Same as [VickyStore::pop_list_head], but `list_key` is typed
    pub fn pop_head<Q: ?Sized + Encode>(&self, list_key: &Q) -> Result<Option<(K, V)>>
    where
        L: Borrow<Q>,
    {
        let list_key = Self::make_list_key(list_key);
        let Some((k, v)) = self.store.owned_pop_list_head(list_key)? else {
            return Ok(None);
        };
        Ok(Some((from_bytes::<K>(&k)?, from_bytes::<V>(&v)?)))
    }

    /// Same as [VickyStore::pop_list_tail], but `list_key` is typed
    pub fn pop_tail<Q: ?Sized + Encode>(&self, list_key: &Q) -> Result<Option<(K, V)>>
    where
        L: Borrow<Q>,
    {
        let list_key = Self::make_list_key(list_key);
        let Some((k, v)) = self.store.owned_pop_list_tail(list_key)? else {
            return Ok(None);
        };
        Ok(Some((from_bytes::<K>(&k)?, from_bytes::<V>(&v)?)))
    }
}

/// A version [VickyTypedList] that's specialized for queues - only allows pushing at the tail and popping
/// from either the head or the tail
#[derive(Clone)]
pub struct VickyTypedQueue<L, V> {
    store: Arc<VickyStore>,
    _phantom: PhantomData<(L, V)>,
}

impl<L, V> VickyTypedQueue<L, V>
where
    L: VickyTypedKey,
    V: Encode + DecodeOwned,
{
    pub fn new(store: Arc<VickyStore>) -> Self {
        Self {
            store,
            _phantom: PhantomData,
        }
    }

    fn make_list_key<Q: ?Sized + Encode>(list_key: &Q) -> Vec<u8>
    where
        L: Borrow<Q>,
    {
        let mut kbytes = list_key.to_bytes::<LE>();
        kbytes.extend_from_slice(&L::TYPE_ID.to_le_bytes());
        kbytes
    }

    /// Pushes a value at the end (tail) of the queue. Returns the auto-generated uuid of the item.
    pub fn push<Q1: ?Sized + Encode, Q2: ?Sized + Encode>(
        &self,
        list_key: &Q1,
        v: &Q2,
    ) -> Result<Uuid>
    where
        L: Borrow<Q1>,
        V: Borrow<Q2>,
    {
        let list_key = Self::make_list_key(list_key);
        let val = v.to_bytes::<LE>();
        self.store.owned_push_to_list(list_key, val)
    }

    /// Pops a value from the beginning (head) of the queue
    pub fn pop_head<Q: ?Sized + Encode>(&self, list_key: &Q) -> Result<Option<V>>
    where
        L: Borrow<Q>,
    {
        let list_key = Self::make_list_key(list_key);
        let Some((_, v)) = self.store.owned_pop_list_head(list_key)? else {
            return Ok(None);
        };
        Ok(Some(from_bytes::<V>(&v)?))
    }

    /// Pops a value from the end (tail) of the queue
    pub fn pop_tail<Q: ?Sized + Encode>(&self, list_key: &Q) -> Result<Option<V>>
    where
        L: Borrow<Q>,
    {
        let list_key = Self::make_list_key(list_key);
        let Some((_, v)) = self.store.owned_pop_list_tail(list_key)? else {
            return Ok(None);
        };
        Ok(Some(from_bytes::<V>(&v)?))
    }
}
