// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{Error, Result};
use libp2p::{
    kad::{
        self, KBucketDistance, PeerRecord, ProgressStep, QueryId, QueryResult, QueryStats, Quorum,
        Record,
    },
    PeerId,
};
use sn_protocol::{storage::RecordHeader, PrettyPrintRecordKey};
use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use tokio::sync::oneshot;
use xor_name::XorName;

use crate::{close_group_majority, GetRecordError, SwarmDriver, CLOSE_GROUP_SIZE};

/// Using XorName to differentiate different record content under the same key.
type GetRecordResultMap = HashMap<XorName, (Record, HashSet<PeerId>)>;
type ExpectedHoldersList = HashSet<PeerId>;
pub(crate) type PendingGetRecord = HashMap<
    QueryId,
    (
        oneshot::Sender<std::result::Result<Record, GetRecordError>>,
        GetRecordResultMap,
        Quorum,
        ExpectedHoldersList,
    ),
>;

// For `get_record` returning behaviour:
//   1, targeting a non-existing entry
//     there will only be one event of `kad::Event::OutboundQueryProgressed`
//     with `ProgressStep::last` to be `true`
//          `QueryStats::requests` to be 20 (K-Value)
//          `QueryStats::success` to be over majority of the requests
//          `err::NotFound::closest_peers` contains a list of CLOSE_GROUP_SIZE peers
//   2, targeting an existing entry
//     there will a sequence of (at least CLOSE_GROUP_SIZE) events of
//     `kad::Event::OutboundQueryProgressed` to be received
//     with `QueryStats::end` always being `None`
//          `ProgressStep::last` all to be `false`
//          `ProgressStep::count` to be increased with step of 1
//             capped and stopped at CLOSE_GROUP_SIZE, may have duplicated counts
//          `PeerRecord::peer` could be None to indicate from self
//             in which case it always use a duplicated `ProgressStep::count`
//     the sequence will be completed with `FinishedWithNoAdditionalRecord`
//     where: `cache_candidates`: being the peers supposed to hold the record but not
//            `ProgressStep::count`: to be `number of received copies plus one`
//            `ProgressStep::last` to be `true`
impl SwarmDriver {
    // Completes when any of the following condition reaches first:
    // 1, Return whenever reached majority of CLOSE_GROUP_SIZE
    // 2, In case of split, return with NotFound,
    //    whenever `ProgressStep::count` hits CLOSE_GROUP_SIZE
    pub(crate) fn accumulate_get_record_found(
        &mut self,
        query_id: QueryId,
        peer_record: PeerRecord,
        stats: QueryStats,
        step: ProgressStep,
    ) -> Result<()> {
        if self.try_early_completion_for_chunk(&query_id, &peer_record)? {
            return Ok(());
        }

        let peer_id = if let Some(peer_id) = peer_record.peer {
            peer_id
        } else {
            self.self_peer_id
        };

        if let Entry::Occupied(mut entry) = self.pending_get_record.entry(query_id) {
            let (_sender, result_map, quorum, expected_holders) = entry.get_mut();

            let pretty_key = PrettyPrintRecordKey::from(&peer_record.record.key).into_owned();

            if !expected_holders.is_empty() {
                if expected_holders.remove(&peer_id) {
                    debug!("For record {pretty_key:?} task {query_id:?}, received a copy from an expected holder {peer_id:?}");
                } else {
                    debug!("For record {pretty_key:?} task {query_id:?}, received a copy from an unexpected holder {peer_id:?}");
                }
            }

            // Insert the record and the peer into the result_map.
            let record_content_hash = XorName::from_content(&peer_record.record.value);
            let responded_peers =
                if let Entry::Occupied(mut entry) = result_map.entry(record_content_hash) {
                    let (_, peer_list) = entry.get_mut();
                    let _ = peer_list.insert(peer_id);
                    peer_list.len()
                } else {
                    let mut peer_list = HashSet::new();
                    let _ = peer_list.insert(peer_id);
                    result_map.insert(record_content_hash, (peer_record.record.clone(), peer_list));
                    1
                };

            let expected_answers = match quorum {
                Quorum::Majority => close_group_majority(),
                Quorum::All => CLOSE_GROUP_SIZE,
                Quorum::N(v) => v.get(),
                Quorum::One => 1,
            };

            trace!("Expecting {expected_answers:?} answers for record {pretty_key:?} task {query_id:?}, received {responded_peers} so far");

            if responded_peers >= expected_answers {
                if !expected_holders.is_empty() {
                    debug!("For record {pretty_key:?} task {query_id:?}, fetch completed with non-responded expected holders {expected_holders:?}");
                }

                // Remove the query task and consume the variables.
                let (sender, result_map, _, _) = entry.remove();

                if result_map.len() == 1 {
                    sender
                        .send(Ok(peer_record.record))
                        .map_err(|_| Error::InternalMsgChannelDropped)?;
                } else {
                    debug!("For record {pretty_key:?} task {query_id:?}, fetch completed with split record");
                    sender
                        .send(Err(GetRecordError::SplitRecord { result_map }))
                        .map_err(|_| Error::InternalMsgChannelDropped)?;
                }

                // Stop the query; possibly stops more nodes from being queried.
                if let Some(mut query) = self.swarm.behaviour_mut().kademlia.query_mut(&query_id) {
                    query.finish();
                }
            } else if usize::from(step.count) >= CLOSE_GROUP_SIZE {
                debug!("For record {pretty_key:?} task {query_id:?}, got {:?} with {} versions so far.",
                   step.count, result_map.len());
            }
        } else {
            // return error if the entry cannot be found
            return Err(Error::ReceivedKademliaEventDropped(
                kad::Event::OutboundQueryProgressed {
                    id: query_id,
                    result: QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(peer_record))),
                    stats,
                    step,
                },
            ));
        }
        Ok(())
    }

    pub(crate) fn handle_get_record_finished(
        &mut self,
        query_id: QueryId,
        cache_candidates: BTreeMap<KBucketDistance, PeerId>,
        stats: QueryStats,
        step: ProgressStep,
    ) -> Result<()> {
        // return error if the entry cannot be found
        let (sender, result_map, _quorum, expected_holders) =
            self.pending_get_record.remove(&query_id).ok_or_else(|| {
                trace!(
                    "Can't locate query task {query_id:?}, it has likely been completed already."
                );
                Error::ReceivedKademliaEventDropped(kad::Event::OutboundQueryProgressed {
                    id: query_id,
                    result: QueryResult::GetRecord(Ok(
                        kad::GetRecordOk::FinishedWithNoAdditionalRecord { cache_candidates },
                    )),
                    stats,
                    step: step.clone(),
                })
            })?;

        let num_of_versions = result_map.len();
        let (result, log_string) = if let Some((record, _)) = result_map.values().next() {
            let result = if num_of_versions == 1 {
                Err(GetRecordError::RecordNotEnoughCopies(record.clone()))
            } else {
                Err(GetRecordError::SplitRecord {
                    result_map: result_map.clone(),
                })
            };

            (result, format!(
                "Getting record {:?} completed with only {:?} copies received, and {num_of_versions} versions.",
                PrettyPrintRecordKey::from(&record.key),
                usize::from(step.count) - 1
            ))
        } else {
            (
                    Err(GetRecordError::RecordNotFound),
                    format!(
                "Getting record task {query_id:?} completed with step count {:?}, but no copy found.",
                step.count
            ),
                )
        };

        if expected_holders.is_empty() {
            debug!("{log_string}");
        } else {
            debug!("{log_string}, and {expected_holders:?} expected holders not responded");
        }

        sender
            .send(result)
            .map_err(|_| Error::InternalMsgChannelDropped)?;

        Ok(())
    }

    pub(crate) fn handle_get_record_error(
        &mut self,
        query_id: QueryId,
        get_record_err: kad::GetRecordError,
        stats: QueryStats,
        step: ProgressStep,
    ) -> Result<()> {
        match &get_record_err {
            kad::GetRecordError::NotFound { .. } => {}
            kad::GetRecordError::QuorumFailed { .. } => {}
            kad::GetRecordError::Timeout { key } => {
                let pretty_key = PrettyPrintRecordKey::from(key);
                let (sender, result_map, quorum, expected_holders) =
                    self.pending_get_record.remove(&query_id).ok_or_else(|| {
                        trace!(
                            "Can't locate query task {query_id:?} for {pretty_key:?}, it has likely been completed already."
                        );
                        Error::ReceivedKademliaEventDropped( kad::Event::OutboundQueryProgressed {
                            id: query_id,
                            result: QueryResult::GetRecord(Err(get_record_err.clone())),
                            stats,
                            step,
                        })
                    })?;

                let required_response_count = match quorum {
                    Quorum::Majority => close_group_majority(),
                    Quorum::All => CLOSE_GROUP_SIZE,
                    Quorum::N(v) => v.into(),
                    Quorum::One => 1,
                };

                // if we've a split over the result xorname, then we don't attempt to resolve this here.
                // Retry and resolve through normal flows without a timeout.
                if result_map.len() > 1 {
                    warn!(
                        "Get record task {query_id:?} for {pretty_key:?} timed out with split result map"
                    );
                    sender
                        .send(Err(GetRecordError::QueryTimeout))
                        .map_err(|_| Error::InternalMsgChannelDropped)?;

                    return Ok(());
                }

                // if we have enough responses here, we can return the record
                if let Some((record, peers)) = result_map.values().next() {
                    if peers.len() >= required_response_count {
                        sender
                            .send(Ok(record.clone()))
                            .map_err(|_| Error::InternalMsgChannelDropped)?;

                        return Ok(());
                    }
                }

                warn!("Get record task {query_id:?} for {pretty_key:?} returned insufficient responses. {expected_holders:?} did not return record");
                // Otherwise report the timeout
                sender
                    .send(Err(GetRecordError::QueryTimeout))
                    .map_err(|_| Error::InternalMsgChannelDropped)?;

                return Ok(());
            }
        }

        // return error if the entry cannot be found
        let (sender, _, _, expected_holders) =
            self.pending_get_record.remove(&query_id).ok_or_else(|| {
                trace!(
                    "Can't locate query task {query_id:?}, it has likely been completed already."
                );
                Error::ReceivedKademliaEventDropped(kad::Event::OutboundQueryProgressed {
                    id: query_id,
                    result: QueryResult::GetRecord(Err(get_record_err.clone())),
                    stats,
                    step,
                })
            })?;
        if expected_holders.is_empty() {
            info!("Get record task {query_id:?} failed with error {get_record_err:?}");
        } else {
            debug!("Get record task {query_id:?} failed with {expected_holders:?} expected holders not responded, error {get_record_err:?}");
        }
        sender
            .send(Err(GetRecordError::RecordNotFound))
            .map_err(|_| Error::InternalMsgChannelDropped)?;
        Ok(())
    }

    // For chunk record which can be self-verifiable,
    // complete the flow with the first copy that fetched.
    // Return `true` if early completed, otherwise return `false`.
    // Situations that can be early completed:
    // 1, Not finding an entry within pending_get_record, i.e. no more further action required
    // 2, For a `Chunk` that not required to verify expected holders,
    //    whenever fetched a first copy that passed the self-verification.
    fn try_early_completion_for_chunk(
        &mut self,
        query_id: &QueryId,
        peer_record: &PeerRecord,
    ) -> Result<bool> {
        if let Entry::Occupied(mut entry) = self.pending_get_record.entry(*query_id) {
            let (_, _, quorum, expected_holders) = entry.get_mut();

            if expected_holders.is_empty() &&
               RecordHeader::is_record_of_type_chunk(&peer_record.record).unwrap_or(false) &&
               // Ensure that we only exit early if quorum is indeed for only one match
               matches!(quorum, Quorum::One)
            {
                // Stop the query; possibly stops more nodes from being queried.
                if let Some(mut query) = self.swarm.behaviour_mut().kademlia.query_mut(query_id) {
                    query.finish();
                }

                // Stop tracking the query task by removing the entry and consume the sender.
                let (sender, ..) = entry.remove();
                // A claimed Chunk type record can be trusted.
                // Punishment of peer that sending corrupted Chunk type record
                // maybe carried out by other verification mechanism.
                sender
                    .send(Ok(peer_record.record.clone()))
                    .map_err(|_| Error::InternalMsgChannelDropped)?;
                return Ok(true);
            }
        } else {
            // A non-existing pending entry does not need to undertake any further action.
            return Ok(true);
        }

        Ok(false)
    }
}