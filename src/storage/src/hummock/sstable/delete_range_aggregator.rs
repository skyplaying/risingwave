// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::future::Future;

#[cfg(test)]
use risingwave_common::util::epoch::is_max_epoch;
use risingwave_hummock_sdk::key::{PointRange, UserKey};
use risingwave_hummock_sdk::HummockEpoch;

use super::MonotonicDeleteEvent;
use crate::hummock::iterator::{DeleteRangeIterator, ForwardMergeRangeIterator};
use crate::hummock::sstable_store::TableHolder;
use crate::hummock::{HummockResult, Sstable};

pub struct CompactionDeleteRangeIterator {
    inner: ForwardMergeRangeIterator,
}

impl CompactionDeleteRangeIterator {
    pub fn new(inner: ForwardMergeRangeIterator) -> Self {
        Self { inner }
    }

    pub async fn next(&mut self) -> HummockResult<()> {
        self.inner.next().await
    }

    #[cfg(test)]
    pub async fn get_tombstone_between(
        self,
        smallest_user_key: UserKey<&[u8]>,
        largest_user_key: UserKey<&[u8]>,
    ) -> HummockResult<Vec<MonotonicDeleteEvent>> {
        let mut iter = self;
        iter.seek(smallest_user_key).await?;
        let extended_smallest_user_key = PointRange::from_user_key(smallest_user_key, false);
        let extended_largest_user_key = PointRange::from_user_key(largest_user_key, false);
        let mut monotonic_events = vec![];
        if !is_max_epoch(iter.earliest_epoch()) {
            monotonic_events.push(MonotonicDeleteEvent {
                event_key: extended_smallest_user_key.to_vec(),
                new_epoch: iter.earliest_epoch(),
            });
        }

        while iter.is_valid() {
            if !extended_largest_user_key.is_empty() && iter.key().ge(&extended_largest_user_key) {
                if !monotonic_events.is_empty() {
                    monotonic_events.push(MonotonicDeleteEvent {
                        event_key: extended_largest_user_key.to_vec(),
                        new_epoch: HummockEpoch::MAX,
                    });
                }
                break;
            }

            let event_key = iter.key().to_vec();
            iter.next().await?;

            monotonic_events.push(MonotonicDeleteEvent {
                new_epoch: iter.earliest_epoch(),
                event_key,
            });
        }

        monotonic_events.dedup_by(|a, b| {
            a.event_key.left_user_key.table_id == b.event_key.left_user_key.table_id
                && a.new_epoch == b.new_epoch
        });
        if !monotonic_events.is_empty() {
            assert!(!is_max_epoch(monotonic_events.first().unwrap().new_epoch));
            assert!(is_max_epoch(monotonic_events.last().unwrap().new_epoch));
        }
        Ok(monotonic_events)
    }

    /// Return the earliest range-tombstone which deletes target-key.
    /// Target-key must be given in order.
    #[cfg(test)]
    pub async fn earliest_delete_which_can_see_key_for_test(
        &mut self,
        target_user_key: UserKey<&[u8]>,
        epoch: HummockEpoch,
    ) -> HummockResult<HummockEpoch> {
        let target_extended_user_key = PointRange::from_user_key(target_user_key, false);
        while self.inner.is_valid()
            && self
                .inner
                .next_extended_user_key()
                .le(&target_extended_user_key)
        {
            self.inner.next().await?;
        }
        Ok(self.earliest_delete_since(epoch))
    }

    pub fn key(&self) -> PointRange<&[u8]> {
        self.inner.next_extended_user_key()
    }

    pub fn is_valid(&self) -> bool {
        self.inner.is_valid()
    }

    pub fn earliest_epoch(&self) -> HummockEpoch {
        self.inner.earliest_epoch()
    }

    pub fn earliest_delete_since(&self, epoch: HummockEpoch) -> HummockEpoch {
        self.inner.earliest_delete_since(epoch)
    }

    /// seek to the first key which larger than `target_user_key`.
    pub async fn seek<'a>(&'a mut self, target_user_key: UserKey<&'a [u8]>) -> HummockResult<()> {
        self.inner.seek(target_user_key).await
    }

    pub async fn rewind(&mut self) -> HummockResult<()> {
        self.inner.rewind().await
    }
}

pub struct SstableDeleteRangeIterator {
    table: TableHolder,
    next_idx: usize,
}

impl SstableDeleteRangeIterator {
    pub fn new(table: TableHolder) -> Self {
        Self { table, next_idx: 0 }
    }

    /// Retrieves whether `next_extended_user_key` is the last range of this SST file.
    ///
    /// Note:
    /// - Before calling this function, makes sure the iterator `is_valid`.
    /// - This function should return immediately.
    ///
    /// # Panics
    /// This function will panic if the iterator is invalid.
    pub fn is_last_range(&self) -> bool {
        debug_assert!(self.next_idx < self.table.meta.monotonic_tombstone_events.len());
        self.next_idx + 1 == self.table.meta.monotonic_tombstone_events.len()
    }
}

impl DeleteRangeIterator for SstableDeleteRangeIterator {
    type NextFuture<'a> = impl Future<Output = HummockResult<()>> + 'a;
    type RewindFuture<'a> = impl Future<Output = HummockResult<()>> + 'a;
    type SeekFuture<'a> = impl Future<Output = HummockResult<()>> + 'a;

    fn next_extended_user_key(&self) -> PointRange<&[u8]> {
        self.table.meta.monotonic_tombstone_events[self.next_idx]
            .event_key
            .as_ref()
    }

    fn current_epoch(&self) -> HummockEpoch {
        if self.next_idx > 0 {
            self.table.meta.monotonic_tombstone_events[self.next_idx - 1].new_epoch
        } else {
            HummockEpoch::MAX
        }
    }

    fn next(&mut self) -> Self::NextFuture<'_> {
        async move {
            self.next_idx += 1;
            Ok(())
        }
    }

    fn rewind(&mut self) -> Self::RewindFuture<'_> {
        async move {
            self.next_idx = 0;
            Ok(())
        }
    }

    fn seek<'a>(&'a mut self, target_user_key: UserKey<&'a [u8]>) -> Self::SeekFuture<'_> {
        async move {
            let target_extended_user_key = PointRange::from_user_key(target_user_key, false);
            self.next_idx = self.table.meta.monotonic_tombstone_events.partition_point(
                |MonotonicDeleteEvent { event_key, .. }| {
                    event_key.as_ref().le(&target_extended_user_key)
                },
            );
            Ok(())
        }
    }

    fn is_valid(&self) -> bool {
        self.next_idx < self.table.meta.monotonic_tombstone_events.len()
    }
}

pub fn get_min_delete_range_epoch_from_sstable(
    table: &Sstable,
    query_user_key: UserKey<&[u8]>,
) -> HummockEpoch {
    let query_extended_user_key = PointRange::from_user_key(query_user_key, false);
    let idx = table.meta.monotonic_tombstone_events.partition_point(
        |MonotonicDeleteEvent { event_key, .. }| event_key.as_ref().le(&query_extended_user_key),
    );
    if idx == 0 {
        HummockEpoch::MAX
    } else {
        table.meta.monotonic_tombstone_events[idx - 1].new_epoch
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Bound;

    use bytes::Bytes;
    use risingwave_common::catalog::TableId;
    use risingwave_common::util::epoch::test_epoch;

    use super::*;
    use crate::hummock::test_utils::delete_range::CompactionDeleteRangesBuilder;
    use crate::hummock::test_utils::test_user_key;

    #[tokio::test]
    pub async fn test_compaction_delete_range_iterator() {
        let mut builder = CompactionDeleteRangesBuilder::default();
        let table_id = TableId::default();
        builder.add_delete_events_for_test(
            9,
            table_id,
            vec![
                (
                    Bound::Included(Bytes::copy_from_slice(b"aaaaaa")),
                    Bound::Excluded(Bytes::copy_from_slice(b"bbbddd")),
                ),
                (
                    Bound::Included(Bytes::copy_from_slice(b"bbbfff")),
                    Bound::Excluded(Bytes::copy_from_slice(b"ffffff")),
                ),
                (
                    Bound::Included(Bytes::copy_from_slice(b"gggggg")),
                    Bound::Excluded(Bytes::copy_from_slice(b"hhhhhh")),
                ),
            ],
        );
        builder.add_delete_events_for_test(
            12,
            table_id,
            vec![(
                Bound::Included(Bytes::copy_from_slice(b"aaaaaa")),
                Bound::Excluded(Bytes::copy_from_slice(b"bbbccc")),
            )],
        );
        builder.add_delete_events_for_test(
            8,
            table_id,
            vec![(
                Bound::Excluded(Bytes::copy_from_slice(b"bbbeee")),
                Bound::Included(Bytes::copy_from_slice(b"eeeeee")),
            )],
        );
        builder.add_delete_events_for_test(
            6,
            table_id,
            vec![(
                Bound::Included(Bytes::copy_from_slice(b"bbbaab")),
                Bound::Excluded(Bytes::copy_from_slice(b"bbbdddf")),
            )],
        );
        builder.add_delete_events_for_test(
            7,
            table_id,
            vec![(
                Bound::Excluded(Bytes::copy_from_slice(b"hhhhhh")),
                Bound::Unbounded,
            )],
        );
        let mut iter = builder.build_for_compaction();
        iter.rewind().await.unwrap();

        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbb").as_ref(),
                test_epoch(13)
            )
            .await
            .unwrap(),
            HummockEpoch::MAX,
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbb").as_ref(),
                test_epoch(11)
            )
            .await
            .unwrap(),
            test_epoch(12)
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbb").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            test_epoch(9)
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbbaaa").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            test_epoch(9)
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbbccd").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            test_epoch(9)
        );

        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbbddd").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            HummockEpoch::MAX,
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbbeee").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            HummockEpoch::MAX,
        );

        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"bbbeef").as_ref(),
                test_epoch(10)
            )
            .await
            .unwrap(),
            HummockEpoch::MAX,
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"eeeeee").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            test_epoch(8)
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"gggggg").as_ref(),
                test_epoch(8)
            )
            .await
            .unwrap(),
            test_epoch(9)
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"hhhhhh").as_ref(),
                test_epoch(6)
            )
            .await
            .unwrap(),
            HummockEpoch::MAX,
        );
        assert_eq!(
            iter.earliest_delete_which_can_see_key_for_test(
                test_user_key(b"iiiiii").as_ref(),
                test_epoch(6)
            )
            .await
            .unwrap(),
            test_epoch(7)
        );
    }

    #[tokio::test]
    pub async fn test_delete_range_split() {
        let table_id = TableId::default();
        let mut builder = CompactionDeleteRangesBuilder::default();
        builder.add_delete_events_for_test(
            13,
            table_id,
            vec![(
                Bound::Included(Bytes::copy_from_slice(b"aaaa")),
                Bound::Excluded(Bytes::copy_from_slice(b"cccc")),
            )],
        );
        builder.add_delete_events_for_test(
            10,
            table_id,
            vec![(
                Bound::Excluded(Bytes::copy_from_slice(b"cccc")),
                Bound::Excluded(Bytes::copy_from_slice(b"dddd")),
            )],
        );
        builder.add_delete_events_for_test(
            12,
            table_id,
            vec![(
                Bound::Included(Bytes::copy_from_slice(b"cccc")),
                Bound::Included(Bytes::copy_from_slice(b"eeee")),
            )],
        );
        builder.add_delete_events_for_test(
            15,
            table_id,
            vec![(
                Bound::Excluded(Bytes::copy_from_slice(b"eeee")),
                Bound::Excluded(Bytes::copy_from_slice(b"ffff")),
            )],
        );
        let compaction_delete_range = builder.build_for_compaction();
        let split_ranges = compaction_delete_range
            .get_tombstone_between(
                test_user_key(b"bbbb").as_ref(),
                test_user_key(b"eeeeee").as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(6, split_ranges.len());
        assert_eq!(
            PointRange::from_user_key(test_user_key(b"bbbb"), false),
            split_ranges[0].event_key
        );
        assert_eq!(
            PointRange::from_user_key(test_user_key(b"cccc"), false),
            split_ranges[1].event_key
        );
        assert_eq!(
            PointRange::from_user_key(test_user_key(b"cccc"), true),
            split_ranges[2].event_key
        );
        assert_eq!(
            PointRange::from_user_key(test_user_key(b"dddd"), false),
            split_ranges[3].event_key
        );
        assert_eq!(
            PointRange::from_user_key(test_user_key(b"eeee"), true),
            split_ranges[4].event_key
        );
        assert_eq!(
            PointRange::from_user_key(test_user_key(b"eeeeee"), false),
            split_ranges[5].event_key
        );
    }
}
