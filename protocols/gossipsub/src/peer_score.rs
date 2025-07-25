// Copyright 2020 Sigma Prime Pty Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Manages and stores the Scoring logic of a particular peer on the gossipsub behaviour.

use std::{
    collections::{hash_map, HashMap, HashSet},
    net::IpAddr,
    time::Duration,
};

use futures_timer::Delay;
use libp2p_identity::PeerId;
use web_time::Instant;

use crate::{time_cache::TimeCache, MessageId, TopicHash};

mod params;
pub use params::{
    score_parameter_decay, score_parameter_decay_with_base, PeerScoreParams, PeerScoreThresholds,
    TopicScoreParams,
};

use crate::ValidationError;

#[cfg(test)]
mod tests;

/// The number of seconds delivery messages are stored in the cache.
const TIME_CACHE_DURATION: u64 = 120;

/// Represents the state of the peer scoring system, which can either be active
/// with a configured `PeerScore`, or disabled entirely.
pub(crate) enum PeerScoreState {
    Active(Box<PeerScore>),
    Disabled,
}

impl PeerScoreState {
    /// Determines if a peer's score is below a given `PeerScoreThreshold` chosen via the
    /// `threshold` parameter.
    pub(crate) fn below_threshold(
        &self,
        peer_id: &PeerId,
        threshold: impl Fn(&PeerScoreThresholds) -> f64,
    ) -> (bool, f64) {
        match self {
            PeerScoreState::Active(active) => {
                let score = active.score_report(peer_id).score;
                (score < threshold(&active.thresholds), score)
            }
            PeerScoreState::Disabled => (false, 0.0),
        }
    }
}

/// Result of a peer score calculation, detailing the peer's
/// computed score and a list of any incurred penalties.
#[derive(Default)]
pub(crate) struct PeerScoreReport {
    pub(crate) score: f64,
    #[cfg(feature = "metrics")]
    pub(crate) penalties: Vec<crate::metrics::Penalty>,
}

pub(crate) struct PeerScore {
    /// The score parameters.
    pub(crate) params: PeerScoreParams,
    /// The score threshold.
    pub(crate) thresholds: PeerScoreThresholds,
    /// The peer score decay interval.
    pub(crate) decay_interval: Delay,
    /// The stats per PeerId.
    peer_stats: HashMap<PeerId, PeerStats>,
    /// Tracking peers per IP.
    peer_ips: HashMap<IpAddr, HashSet<PeerId>>,
    /// Message delivery tracking. This is a time-cache of [`DeliveryRecord`]s.
    deliveries: TimeCache<MessageId, DeliveryRecord>,
    /// Callback for monitoring message delivery times.
    message_delivery_time_callback: Option<fn(&PeerId, &TopicHash, f64)>,
}

/// General statistics for a given gossipsub peer.
struct PeerStats {
    /// Connection status of the peer.
    status: ConnectionStatus,
    /// Stats per topic.
    topics: HashMap<TopicHash, TopicStats>,
    /// IP tracking for individual peers.
    known_ips: HashSet<IpAddr>,
    /// Behaviour penalty that is applied to the peer, assigned by the behaviour.
    behaviour_penalty: f64,
    /// Application specific score. Can be manipulated by calling PeerScore::set_application_score
    application_score: f64,
    /// Scoring based on how whether this peer consumes messages fast enough or not.
    slow_peer_penalty: f64,
}

enum ConnectionStatus {
    /// The peer is connected.
    Connected,
    /// The peer is disconnected
    Disconnected {
        /// Expiration time of the score state for disconnected peers.
        expire: Instant,
    },
}

impl Default for PeerStats {
    fn default() -> Self {
        PeerStats {
            status: ConnectionStatus::Connected,
            topics: HashMap::new(),
            known_ips: HashSet::new(),
            behaviour_penalty: 0f64,
            application_score: 0f64,
            slow_peer_penalty: 0f64,
        }
    }
}

impl PeerStats {
    /// Returns a mutable reference to topic stats if they exist, otherwise if the supplied
    /// parameters score the topic, inserts the default stats and returns a reference to those.
    /// If neither apply, returns None.
    pub(crate) fn stats_or_default_mut(
        &mut self,
        topic_hash: TopicHash,
        params: &PeerScoreParams,
    ) -> Option<&mut TopicStats> {
        #[allow(
            clippy::map_entry,
            reason = "False positive, see rust-lang/rust-clippy#14449."
        )]
        if params.topics.contains_key(&topic_hash) {
            Some(self.topics.entry(topic_hash).or_default())
        } else {
            self.topics.get_mut(&topic_hash)
        }
    }
}

/// Stats assigned to peer for each topic.
struct TopicStats {
    mesh_status: MeshStatus,
    /// Number of first message deliveries.
    first_message_deliveries: f64,
    /// True if the peer has been in the mesh for enough time to activate mesh message deliveries.
    mesh_message_deliveries_active: bool,
    /// Number of message deliveries from the mesh.
    mesh_message_deliveries: f64,
    /// Mesh rate failure penalty.
    mesh_failure_penalty: f64,
    /// Invalid message counter.
    invalid_message_deliveries: f64,
}

impl TopicStats {
    /// Returns true if the peer is in the `mesh`.
    pub(crate) fn in_mesh(&self) -> bool {
        matches!(self.mesh_status, MeshStatus::Active { .. })
    }
}

/// Status defining a peer's inclusion in the mesh and associated parameters.
enum MeshStatus {
    Active {
        /// The time the peer was last GRAFTed;
        graft_time: Instant,
        /// The time the peer has been in the mesh.
        mesh_time: Duration,
    },
    InActive,
}

impl MeshStatus {
    /// Initialises a new [`MeshStatus::Active`] mesh status.
    pub(crate) fn new_active() -> Self {
        MeshStatus::Active {
            graft_time: Instant::now(),
            mesh_time: Duration::from_secs(0),
        }
    }
}

impl Default for TopicStats {
    fn default() -> Self {
        TopicStats {
            mesh_status: MeshStatus::InActive,
            first_message_deliveries: Default::default(),
            mesh_message_deliveries_active: Default::default(),
            mesh_message_deliveries: Default::default(),
            mesh_failure_penalty: Default::default(),
            invalid_message_deliveries: Default::default(),
        }
    }
}

#[derive(PartialEq, Debug)]
struct DeliveryRecord {
    status: DeliveryStatus,
    first_seen: Instant,
    peers: HashSet<PeerId>,
}

#[derive(PartialEq, Debug)]
enum DeliveryStatus {
    /// Don't know (yet) if the message is valid.
    Unknown,
    /// The message is valid together with the validated time.
    Valid(Instant),
    /// The message is invalid.
    Invalid,
    /// Instructed by the validator to ignore the message.
    Ignored,
}

impl Default for DeliveryRecord {
    fn default() -> Self {
        DeliveryRecord {
            status: DeliveryStatus::Unknown,
            first_seen: Instant::now(),
            peers: HashSet::new(),
        }
    }
}

impl PeerScore {
    /// Creates a new [`PeerScore`] using a given set of peer scoring parameters.
    #[allow(dead_code)]
    pub(crate) fn new(params: PeerScoreParams, thresholds: PeerScoreThresholds) -> Self {
        Self::new_with_message_delivery_time_callback(params, thresholds, None)
    }

    pub(crate) fn new_with_message_delivery_time_callback(
        params: PeerScoreParams,
        thresholds: PeerScoreThresholds,
        callback: Option<fn(&PeerId, &TopicHash, f64)>,
    ) -> Self {
        PeerScore {
            decay_interval: Delay::new(params.decay_interval),
            params,
            thresholds,
            peer_stats: HashMap::new(),
            peer_ips: HashMap::new(),
            deliveries: TimeCache::new(Duration::from_secs(TIME_CACHE_DURATION)),
            message_delivery_time_callback: callback,
        }
    }

    /// Returns the score report for a peer, with applied penalties.
    /// This is called from the heartbeat
    pub(crate) fn score_report(&self, peer_id: &PeerId) -> PeerScoreReport {
        let mut report = PeerScoreReport::default();
        let Some(peer_stats) = self.peer_stats.get(peer_id) else {
            return report;
        };

        // topic scores
        for (topic, topic_stats) in peer_stats.topics.iter() {
            // topic parameters
            if let Some(topic_params) = self.params.topics.get(topic) {
                // we are tracking the topic

                // the topic score
                let mut topic_score = 0.0;

                // P1: time in mesh
                if let MeshStatus::Active { mesh_time, .. } = topic_stats.mesh_status {
                    let p1 = {
                        let v = mesh_time.as_secs_f64()
                            / topic_params.time_in_mesh_quantum.as_secs_f64();
                        if v < topic_params.time_in_mesh_cap {
                            v
                        } else {
                            topic_params.time_in_mesh_cap
                        }
                    };
                    topic_score += p1 * topic_params.time_in_mesh_weight;
                }

                // P2: first message deliveries
                let p2 = {
                    let v = topic_stats.first_message_deliveries;
                    if v < topic_params.first_message_deliveries_cap {
                        v
                    } else {
                        topic_params.first_message_deliveries_cap
                    }
                };
                topic_score += p2 * topic_params.first_message_deliveries_weight;

                // P3: mesh message deliveries
                if topic_stats.mesh_message_deliveries_active
                    && topic_stats.mesh_message_deliveries
                        < topic_params.mesh_message_deliveries_threshold
                    && topic_params.mesh_message_deliveries_weight != 0.0
                {
                    let deficit = topic_params.mesh_message_deliveries_threshold
                        - topic_stats.mesh_message_deliveries;
                    let p3 = deficit * deficit;
                    let penalty = p3 * topic_params.mesh_message_deliveries_weight;

                    topic_score += penalty;
                    #[cfg(feature = "metrics")]
                    report
                        .penalties
                        .push(crate::metrics::Penalty::MessageDeficit);
                    tracing::debug!(
                        peer=%peer_id,
                        %topic,
                        %deficit,
                        penalty=%penalty,
                        "[Penalty] The peer has a mesh deliveries deficit and will be penalized"
                    );
                }

                // P3b:
                // NOTE: the weight of P3b is negative (validated in TopicScoreParams.validate), so
                // this detracts.
                let p3b = topic_stats.mesh_failure_penalty;
                topic_score += p3b * topic_params.mesh_failure_penalty_weight;

                // P4: invalid messages
                // NOTE: the weight of P4 is negative (validated in TopicScoreParams.validate), so
                // this detracts.
                let p4 =
                    topic_stats.invalid_message_deliveries * topic_stats.invalid_message_deliveries;
                topic_score += p4 * topic_params.invalid_message_deliveries_weight;

                // update score, mixing with topic weight
                report.score += topic_score * topic_params.topic_weight;
            }
        }

        // apply the topic score cap, if any
        if self.params.topic_score_cap > 0f64 && report.score > self.params.topic_score_cap {
            report.score = self.params.topic_score_cap;
        }

        // P5: application-specific score
        let p5 = peer_stats.application_score;
        report.score += p5 * self.params.app_specific_weight;

        // P6: IP collocation factor
        for ip in peer_stats.known_ips.iter() {
            if self.params.ip_colocation_factor_whitelist.contains(ip) {
                continue;
            }

            // P6 has a cliff (ip_colocation_factor_threshold); it's only applied if
            // at least that many peers are connected to us from that source IP
            // addr. It is quadratic, and the weight is negative (validated by
            // peer_score_params.validate()).
            if let Some(peers_in_ip) = self.peer_ips.get(ip).map(|peers| peers.len()) {
                if (peers_in_ip as f64) > self.params.ip_colocation_factor_threshold
                    && self.params.ip_colocation_factor_weight != 0.0
                {
                    let surplus = (peers_in_ip as f64) - self.params.ip_colocation_factor_threshold;
                    let p6 = surplus * surplus;
                    #[cfg(feature = "metrics")]
                    report.penalties.push(crate::metrics::Penalty::IPColocation);
                    tracing::debug!(
                        peer=%peer_id,
                        surplus_ip=%ip,
                        surplus=%surplus,
                        "[Penalty] The peer gets penalized because of too many peers with the same ip"
                    );
                    report.score += p6 * self.params.ip_colocation_factor_weight;
                }
            }
        }

        // P7: behavioural pattern penalty.
        if peer_stats.behaviour_penalty > self.params.behaviour_penalty_threshold {
            let excess = peer_stats.behaviour_penalty - self.params.behaviour_penalty_threshold;
            let p7 = excess * excess;
            report.score += p7 * self.params.behaviour_penalty_weight;
        }

        // Slow peer weighting.
        if peer_stats.slow_peer_penalty > self.params.slow_peer_threshold {
            let excess = peer_stats.slow_peer_penalty - self.params.slow_peer_threshold;
            report.score += excess * self.params.slow_peer_weight;
        }

        report
    }

    pub(crate) fn add_penalty(&mut self, peer_id: &PeerId, count: usize) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            tracing::debug!(
                peer=%peer_id,
                %count,
                "[Penalty] Behavioral penalty for peer"
            );
            peer_stats.behaviour_penalty += count as f64;
        }
    }

    fn remove_ips_for_peer(
        peer_stats: &PeerStats,
        peer_ips: &mut HashMap<IpAddr, HashSet<PeerId>>,
        peer_id: &PeerId,
    ) {
        for ip in peer_stats.known_ips.iter() {
            if let Some(peer_set) = peer_ips.get_mut(ip) {
                peer_set.remove(peer_id);
            }
        }
    }

    pub(crate) fn refresh_scores(&mut self) {
        let now = Instant::now();
        let params_ref = &self.params;
        let peer_ips_ref = &mut self.peer_ips;
        self.peer_stats.retain(|peer_id, peer_stats| {
            if let ConnectionStatus::Disconnected { expire } = peer_stats.status {
                // has the retention period expired?
                if now > expire {
                    // yes, throw it away (but clean up the IP tracking first)
                    Self::remove_ips_for_peer(peer_stats, peer_ips_ref, peer_id);
                    // re address this, use retain or entry
                    return false;
                }

                // we don't decay retained scores, as the peer is not active.
                // this way the peer cannot reset a negative score by simply disconnecting and
                // reconnecting, unless the retention period has elapsed.
                // similarly, a well behaved peer does not lose its score by getting disconnected.
                return true;
            }

            for (topic, topic_stats) in peer_stats.topics.iter_mut() {
                // the topic parameters
                if let Some(topic_params) = params_ref.topics.get(topic) {
                    // decay counters
                    topic_stats.first_message_deliveries *=
                        topic_params.first_message_deliveries_decay;
                    if topic_stats.first_message_deliveries < params_ref.decay_to_zero {
                        topic_stats.first_message_deliveries = 0.0;
                    }
                    topic_stats.mesh_message_deliveries *=
                        topic_params.mesh_message_deliveries_decay;
                    if topic_stats.mesh_message_deliveries < params_ref.decay_to_zero {
                        topic_stats.mesh_message_deliveries = 0.0;
                    }
                    topic_stats.mesh_failure_penalty *= topic_params.mesh_failure_penalty_decay;
                    if topic_stats.mesh_failure_penalty < params_ref.decay_to_zero {
                        topic_stats.mesh_failure_penalty = 0.0;
                    }
                    topic_stats.invalid_message_deliveries *=
                        topic_params.invalid_message_deliveries_decay;
                    if topic_stats.invalid_message_deliveries < params_ref.decay_to_zero {
                        topic_stats.invalid_message_deliveries = 0.0;
                    }
                    // update mesh time and activate mesh message delivery parameter if need be
                    if let MeshStatus::Active {
                        ref mut mesh_time,
                        ref mut graft_time,
                    } = topic_stats.mesh_status
                    {
                        *mesh_time = now.duration_since(*graft_time);
                        if *mesh_time > topic_params.mesh_message_deliveries_activation {
                            topic_stats.mesh_message_deliveries_active = true;
                        }
                    }
                }
            }

            // decay P7 counter
            peer_stats.behaviour_penalty *= params_ref.behaviour_penalty_decay;
            if peer_stats.behaviour_penalty < params_ref.decay_to_zero {
                peer_stats.behaviour_penalty = 0.0;
            }

            // decay slow peer score
            peer_stats.slow_peer_penalty *= params_ref.slow_peer_decay;
            if peer_stats.slow_peer_penalty < params_ref.decay_to_zero {
                peer_stats.slow_peer_penalty = 0.0;
            }

            true
        });
    }

    /// Adds a connected peer to [`PeerScore`], initialising with empty ips (ips get added later
    /// through add_ip.
    pub(crate) fn add_peer(&mut self, peer_id: PeerId) {
        let peer_stats = self.peer_stats.entry(peer_id).or_default();

        // mark the peer as connected
        peer_stats.status = ConnectionStatus::Connected;
    }

    /// Adds a new ip to a peer, if the peer is not yet known creates a new peer_stats entry for it
    pub(crate) fn add_ip(&mut self, peer_id: &PeerId, ip: IpAddr) {
        tracing::trace!(peer=%peer_id, %ip, "Add ip for peer");
        let peer_stats = self.peer_stats.entry(*peer_id).or_default();

        // Mark the peer as connected (currently the default is connected, but we don't want to
        // rely on the default).
        peer_stats.status = ConnectionStatus::Connected;

        // Insert the ip
        peer_stats.known_ips.insert(ip);
        self.peer_ips.entry(ip).or_default().insert(*peer_id);
    }

    /// Indicate that a peer has been too slow to consume a message.
    pub(crate) fn failed_message_slow_peer(&mut self, peer_id: &PeerId) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            peer_stats.slow_peer_penalty += 1.0;
            tracing::debug!(peer=%peer_id, %peer_stats.slow_peer_penalty, "[Penalty] Expired message penalty.");
        }
    }

    /// Removes an ip from a peer
    pub(crate) fn remove_ip(&mut self, peer_id: &PeerId, ip: &IpAddr) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            peer_stats.known_ips.remove(ip);
            if let Some(peer_ids) = self.peer_ips.get_mut(ip) {
                tracing::trace!(peer=%peer_id, %ip, "Remove ip for peer");
                peer_ids.remove(peer_id);
            } else {
                tracing::trace!(
                    peer=%peer_id,
                    %ip,
                    "No entry in peer_ips for ip which should get removed for peer"
                );
            }
        } else {
            tracing::trace!(
                peer=%peer_id,
                %ip,
                "No peer_stats for peer which should remove the ip"
            );
        }
    }

    /// Removes a peer from the score table. This retains peer statistics if their score is
    /// non-positive.
    pub(crate) fn remove_peer(&mut self, peer_id: &PeerId) {
        // we only retain non-positive scores of peers
        if self.score_report(peer_id).score > 0f64 {
            if let hash_map::Entry::Occupied(entry) = self.peer_stats.entry(*peer_id) {
                Self::remove_ips_for_peer(entry.get(), &mut self.peer_ips, peer_id);
                entry.remove();
            }
            return;
        }

        // if the peer is retained (including it's score) the `first_message_delivery` counters
        // are reset to 0 and mesh delivery penalties applied.
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            for (topic, topic_stats) in peer_stats.topics.iter_mut() {
                topic_stats.first_message_deliveries = 0f64;

                if let Some(threshold) = self
                    .params
                    .topics
                    .get(topic)
                    .map(|param| param.mesh_message_deliveries_threshold)
                {
                    if topic_stats.in_mesh()
                        && topic_stats.mesh_message_deliveries_active
                        && topic_stats.mesh_message_deliveries < threshold
                    {
                        let deficit = threshold - topic_stats.mesh_message_deliveries;
                        topic_stats.mesh_failure_penalty += deficit * deficit;
                    }
                }

                topic_stats.mesh_status = MeshStatus::InActive;
                topic_stats.mesh_message_deliveries_active = false;
            }

            peer_stats.status = ConnectionStatus::Disconnected {
                expire: Instant::now() + self.params.retain_score,
            };
        }
    }

    /// Handles scoring functionality as a peer GRAFTs to a topic.
    pub(crate) fn graft(&mut self, peer_id: &PeerId, topic: impl Into<TopicHash>) {
        let topic = topic.into();
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            // if we are scoring the topic, update the mesh status.
            if let Some(topic_stats) = peer_stats.stats_or_default_mut(topic, &self.params) {
                topic_stats.mesh_status = MeshStatus::new_active();
                topic_stats.mesh_message_deliveries_active = false;
            }
        }
    }

    /// Handles scoring functionality as a peer PRUNEs from a topic.
    pub(crate) fn prune(&mut self, peer_id: &PeerId, topic: TopicHash) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            // if we are scoring the topic, update the mesh status.
            if let Some(topic_stats) = peer_stats.stats_or_default_mut(topic.clone(), &self.params)
            {
                // sticky mesh delivery rate failure penalty
                let threshold = self
                    .params
                    .topics
                    .get(&topic)
                    .expect("Topic must exist in order for there to be topic stats")
                    .mesh_message_deliveries_threshold;
                if topic_stats.mesh_message_deliveries_active
                    && topic_stats.mesh_message_deliveries < threshold
                {
                    let deficit = threshold - topic_stats.mesh_message_deliveries;
                    topic_stats.mesh_failure_penalty += deficit * deficit;
                }
                topic_stats.mesh_message_deliveries_active = false;
                topic_stats.mesh_status = MeshStatus::InActive;
            }
        }
    }

    pub(crate) fn validate_message(
        &mut self,
        from: &PeerId,
        msg_id: &MessageId,
        topic_hash: &TopicHash,
    ) {
        // adds an empty record with the message id
        self.deliveries.entry(msg_id.clone()).or_default();

        if let Some(callback) = self.message_delivery_time_callback {
            if self
                .peer_stats
                .get(from)
                .and_then(|s| s.topics.get(topic_hash))
                .map(|ts| ts.in_mesh())
                .unwrap_or(false)
            {
                callback(from, topic_hash, 0.0);
            }
        }
    }

    pub(crate) fn deliver_message(
        &mut self,
        from: &PeerId,
        msg_id: &MessageId,
        topic_hash: &TopicHash,
    ) {
        self.mark_first_message_delivery(from, topic_hash);

        let record = self.deliveries.entry(msg_id.clone()).or_default();

        // this should be the first delivery trace
        if record.status != DeliveryStatus::Unknown {
            tracing::warn!(
                peer=%from,
                status=?record.status,
                first_seen=?record.first_seen.elapsed().as_secs(),
                "Unexpected delivery trace"
            );
            return;
        }

        // mark the message as valid and reward mesh peers that have already forwarded it to us
        record.status = DeliveryStatus::Valid(Instant::now());
        for peer in record.peers.iter().cloned().collect::<Vec<_>>() {
            // this check is to make sure a peer can't send us a message twice and get a double
            // count if it is a first delivery
            if &peer != from {
                self.mark_duplicate_message_delivery(&peer, topic_hash, None);
            }
        }
    }

    /// Similar to `reject_message` except does not require the message id or reason for an invalid
    /// message.
    pub(crate) fn reject_invalid_message(&mut self, from: &PeerId, topic_hash: &TopicHash) {
        tracing::debug!(
            peer=%from,
            "[Penalty] Message from peer rejected because of ValidationError or SelfOrigin"
        );

        self.mark_invalid_message_delivery(from, topic_hash);
    }

    // Reject a message.
    pub(crate) fn reject_message(
        &mut self,
        from: &PeerId,
        msg_id: &MessageId,
        topic_hash: &TopicHash,
        reason: RejectReason,
    ) {
        match reason {
            // these messages are not tracked, but the peer is penalized as they are invalid
            RejectReason::ValidationError(_) | RejectReason::SelfOrigin => {
                self.reject_invalid_message(from, topic_hash);
                return;
            }
            // we ignore those messages, so do nothing.
            RejectReason::BlackListedPeer | RejectReason::BlackListedSource => {
                return;
            }
            _ => {} // the rest are handled after record creation
        }

        let peers: Vec<_> = {
            let record = self.deliveries.entry(msg_id.clone()).or_default();

            // Multiple peers can now reject the same message as we track which peers send us the
            // message. If we have already updated the status, return.
            if record.status != DeliveryStatus::Unknown {
                return;
            }

            if let RejectReason::ValidationIgnored = reason {
                // we were explicitly instructed by the validator to ignore the message but not
                // penalize the peer
                record.status = DeliveryStatus::Ignored;
                record.peers.clear();
                return;
            }

            // mark the message as invalid and penalize peers that have already forwarded it.
            record.status = DeliveryStatus::Invalid;
            // release the delivery time tracking map to free some memory early
            record.peers.drain().collect()
        };

        self.mark_invalid_message_delivery(from, topic_hash);
        for peer_id in peers.iter() {
            self.mark_invalid_message_delivery(peer_id, topic_hash)
        }
    }

    pub(crate) fn duplicated_message(
        &mut self,
        from: &PeerId,
        msg_id: &MessageId,
        topic_hash: &TopicHash,
    ) {
        let record = self.deliveries.entry(msg_id.clone()).or_default();

        if record.peers.contains(from) {
            // we have already seen this duplicate!
            return;
        }

        if let Some(callback) = self.message_delivery_time_callback {
            let time = if let DeliveryStatus::Valid(validated) = record.status {
                validated.elapsed().as_secs_f64()
            } else {
                0.0
            };
            if self
                .peer_stats
                .get(from)
                .and_then(|s| s.topics.get(topic_hash))
                .map(|ts| ts.in_mesh())
                .unwrap_or(false)
            {
                callback(from, topic_hash, time);
            }
        }

        match record.status {
            DeliveryStatus::Unknown => {
                // the message is being validated; track the peer delivery and wait for
                // the Deliver/Reject notification.
                record.peers.insert(*from);
            }
            DeliveryStatus::Valid(validated) => {
                // mark the peer delivery time to only count a duplicate delivery once.
                record.peers.insert(*from);
                self.mark_duplicate_message_delivery(from, topic_hash, Some(validated));
            }
            DeliveryStatus::Invalid => {
                // we no longer track delivery time
                self.mark_invalid_message_delivery(from, topic_hash);
            }
            DeliveryStatus::Ignored => {
                // the message was ignored; do nothing (we don't know if it was valid)
            }
        }
    }

    /// Sets the application specific score for a peer. Returns true if the peer is the peer is
    /// connected or if the score of the peer is not yet expired and false otherwise.
    pub(crate) fn set_application_score(&mut self, peer_id: &PeerId, new_score: f64) -> bool {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            peer_stats.application_score = new_score;
            true
        } else {
            false
        }
    }

    /// Sets scoring parameters for a topic.
    pub(crate) fn set_topic_params(&mut self, topic_hash: TopicHash, params: TopicScoreParams) {
        use hash_map::Entry::*;
        match self.params.topics.entry(topic_hash.clone()) {
            Occupied(mut entry) => {
                let first_message_deliveries_cap = params.first_message_deliveries_cap;
                let mesh_message_deliveries_cap = params.mesh_message_deliveries_cap;
                let old_params = entry.insert(params);

                if old_params.first_message_deliveries_cap > first_message_deliveries_cap {
                    for stats in &mut self.peer_stats.values_mut() {
                        if let Some(tstats) = stats.topics.get_mut(&topic_hash) {
                            if tstats.first_message_deliveries > first_message_deliveries_cap {
                                tstats.first_message_deliveries = first_message_deliveries_cap;
                            }
                        }
                    }
                }

                if old_params.mesh_message_deliveries_cap > mesh_message_deliveries_cap {
                    for stats in self.peer_stats.values_mut() {
                        if let Some(tstats) = stats.topics.get_mut(&topic_hash) {
                            if tstats.mesh_message_deliveries > mesh_message_deliveries_cap {
                                tstats.mesh_message_deliveries = mesh_message_deliveries_cap;
                            }
                        }
                    }
                }
            }
            Vacant(entry) => {
                entry.insert(params);
            }
        }
    }

    /// Returns a scoring parameters for a topic if existent.
    pub(crate) fn get_topic_params(&self, topic_hash: &TopicHash) -> Option<&TopicScoreParams> {
        self.params.topics.get(topic_hash)
    }

    /// Increments the "invalid message deliveries" counter for all scored topics the message
    /// is published in.
    fn mark_invalid_message_delivery(&mut self, peer_id: &PeerId, topic_hash: &TopicHash) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            if let Some(topic_stats) =
                peer_stats.stats_or_default_mut(topic_hash.clone(), &self.params)
            {
                tracing::debug!(
                    peer=%peer_id,
                    topic=%topic_hash,
                    "[Penalty] Peer delivered an invalid message in topic and gets penalized \
                    for it",
                );
                topic_stats.invalid_message_deliveries += 1f64;
            }
        }
    }

    /// Increments the "first message deliveries" counter for all scored topics the message is
    /// published in, as well as the "mesh message deliveries" counter, if the peer is in the
    /// mesh for the topic.
    fn mark_first_message_delivery(&mut self, peer_id: &PeerId, topic_hash: &TopicHash) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            if let Some(topic_stats) =
                peer_stats.stats_or_default_mut(topic_hash.clone(), &self.params)
            {
                let cap = self
                    .params
                    .topics
                    .get(topic_hash)
                    .expect("Topic must exist if there are known topic_stats")
                    .first_message_deliveries_cap;
                topic_stats.first_message_deliveries =
                    if topic_stats.first_message_deliveries + 1f64 > cap {
                        cap
                    } else {
                        topic_stats.first_message_deliveries + 1f64
                    };

                if let MeshStatus::Active { .. } = topic_stats.mesh_status {
                    let cap = self
                        .params
                        .topics
                        .get(topic_hash)
                        .expect("Topic must exist if there are known topic_stats")
                        .mesh_message_deliveries_cap;

                    topic_stats.mesh_message_deliveries =
                        if topic_stats.mesh_message_deliveries + 1f64 > cap {
                            cap
                        } else {
                            topic_stats.mesh_message_deliveries + 1f64
                        };
                }
            }
        }
    }

    /// Increments the "mesh message deliveries" counter for messages we've seen before, as long the
    /// message was received within the P3 window.
    fn mark_duplicate_message_delivery(
        &mut self,
        peer_id: &PeerId,
        topic_hash: &TopicHash,
        validated_time: Option<Instant>,
    ) {
        if let Some(peer_stats) = self.peer_stats.get_mut(peer_id) {
            let now = if validated_time.is_some() {
                Some(Instant::now())
            } else {
                None
            };
            if let Some(topic_stats) =
                peer_stats.stats_or_default_mut(topic_hash.clone(), &self.params)
            {
                if let MeshStatus::Active { .. } = topic_stats.mesh_status {
                    let topic_params = self
                        .params
                        .topics
                        .get(topic_hash)
                        .expect("Topic must exist if there are known topic_stats");

                    // check against the mesh delivery window -- if the validated time is passed as
                    // 0, then the message was received before we finished
                    // validation and thus falls within the mesh
                    // delivery window.
                    let mut falls_in_mesh_deliver_window = true;
                    if let Some(validated_time) = validated_time {
                        if let Some(now) = &now {
                            // should always be true
                            let window_time = validated_time
                                .checked_add(topic_params.mesh_message_deliveries_window)
                                .unwrap_or(*now);
                            if now > &window_time {
                                falls_in_mesh_deliver_window = false;
                            }
                        }
                    }

                    if falls_in_mesh_deliver_window {
                        let cap = topic_params.mesh_message_deliveries_cap;
                        topic_stats.mesh_message_deliveries =
                            if topic_stats.mesh_message_deliveries + 1f64 > cap {
                                cap
                            } else {
                                topic_stats.mesh_message_deliveries + 1f64
                            };
                    }
                }
            }
        }
    }

    pub(crate) fn mesh_message_deliveries(&self, peer: &PeerId, topic: &TopicHash) -> Option<f64> {
        self.peer_stats
            .get(peer)
            .and_then(|s| s.topics.get(topic))
            .map(|t| t.mesh_message_deliveries)
    }
}

/// The reason a Gossipsub message has been rejected.
#[derive(Clone, Copy)]
pub(crate) enum RejectReason {
    /// The message failed the configured validation during decoding.
    ValidationError(ValidationError),
    /// The message source is us.
    SelfOrigin,
    /// The peer that sent the message was blacklisted.
    BlackListedPeer,
    /// The source (from field) of the message was blacklisted.
    BlackListedSource,
    /// The validation was ignored.
    ValidationIgnored,
    /// The validation failed.
    ValidationFailed,
}
