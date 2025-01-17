// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Statement handling to plug on top of the network service.
//!
//! Usage:
//!
//! - Use [`StatementHandlerPrototype::new`] to create a prototype.
//! - Pass the `NonDefaultSetConfig` returned from [`StatementHandlerPrototype::new`] to the network
//!   configuration as an extra peers set.
//! - Use [`StatementHandlerPrototype::build`] then [`StatementHandler::run`] to obtain a
//! `Future` that processes statements.

use crate::config::*;

use codec::{Decode, Encode};
use futures::{channel::oneshot, prelude::*, stream::FuturesUnordered, FutureExt};
use prometheus_endpoint::{register, Counter, PrometheusError, Registry, U64};
use sc_network::{
	config::{NonReservedPeerMode, SetConfig},
	error,
	multiaddr::{Multiaddr, Protocol},
	peer_store::PeerStoreProvider,
	service::{
		traits::{NotificationEvent, NotificationService, ValidationResult},
		NotificationMetrics,
	},
	types::ProtocolName,
	utils::{interval, LruHashSet},
	NetworkBackend, NetworkEventStream, NetworkPeers,
};
use sc_network_common::role::ObservedRole;
use sc_network_sync::{SyncEvent, SyncEventStream};
use sc_network_types::PeerId;
use sp_runtime::traits::Block as BlockT;
use sp_statement_store::{
	Hash, NetworkPriority, Statement, StatementSource, StatementStore, SubmitResult,
};
use std::{
	collections::{hash_map::Entry, HashMap, HashSet},
	iter,
	num::NonZeroUsize,
	pin::Pin,
	sync::Arc,
};

pub mod config;

/// A set of statements.
pub type Statements = Vec<Statement>;
/// Future resolving to statement import result.
pub type StatementImportFuture = oneshot::Receiver<SubmitResult>;

mod rep {
	use sc_network::ReputationChange as Rep;
	/// Reputation change when a peer sends us any statement.
	///
	/// This forces node to verify it, thus the negative value here. Once statement is verified,
	/// reputation change should be refunded with `ANY_STATEMENT_REFUND`
	pub const ANY_STATEMENT: Rep = Rep::new(-(1 << 4), "Any statement");
	/// Reputation change when a peer sends us any statement that is not invalid.
	pub const ANY_STATEMENT_REFUND: Rep = Rep::new(1 << 4, "Any statement (refund)");
	/// Reputation change when a peer sends us an statement that we didn't know about.
	pub const GOOD_STATEMENT: Rep = Rep::new(1 << 7, "Good statement");
	/// Reputation change when a peer sends us a bad statement.
	pub const BAD_STATEMENT: Rep = Rep::new(-(1 << 12), "Bad statement");
	/// Reputation change when a peer sends us a duplicate statement.
	pub const DUPLICATE_STATEMENT: Rep = Rep::new(-(1 << 7), "Duplicate statement");
	/// Reputation change when a peer sends us particularly useful statement
	pub const EXCELLENT_STATEMENT: Rep = Rep::new(1 << 8, "High priority statement");
}

const LOG_TARGET: &str = "statement-gossip";

struct Metrics {
	propagated_statements: Counter<U64>,
}

impl Metrics {
	fn register(r: &Registry) -> Result<Self, PrometheusError> {
		Ok(Self {
			propagated_statements: register(
				Counter::new(
					"substrate_sync_propagated_statements",
					"Number of statements propagated to at least one peer",
				)?,
				r,
			)?,
		})
	}
}

/// Prototype for a [`StatementHandler`].
pub struct StatementHandlerPrototype {
	protocol_name: ProtocolName,
	notification_service: Box<dyn NotificationService>,
}

impl StatementHandlerPrototype {
	/// Create a new instance.
	pub fn new<
		Hash: AsRef<[u8]>,
		Block: BlockT,
		Net: NetworkBackend<Block, <Block as BlockT>::Hash>,
	>(
		genesis_hash: Hash,
		fork_id: Option<&str>,
		metrics: NotificationMetrics,
		peer_store_handle: Arc<dyn PeerStoreProvider>,
	) -> (Self, Net::NotificationProtocolConfig) {
		let genesis_hash = genesis_hash.as_ref();
		let protocol_name = if let Some(fork_id) = fork_id {
			format!("/{}/{}/statement/1", array_bytes::bytes2hex("", genesis_hash), fork_id)
		} else {
			format!("/{}/statement/1", array_bytes::bytes2hex("", genesis_hash))
		};
		let (config, notification_service) = Net::notification_config(
			protocol_name.clone().into(),
			Vec::new(),
			MAX_STATEMENT_SIZE,
			None,
			SetConfig {
				in_peers: 0,
				out_peers: 0,
				reserved_nodes: Vec::new(),
				non_reserved_mode: NonReservedPeerMode::Deny,
			},
			metrics,
			peer_store_handle,
		);

		(Self { protocol_name: protocol_name.into(), notification_service }, config)
	}

	/// Turns the prototype into the actual handler.
	///
	/// Important: the statements handler is initially disabled and doesn't gossip statements.
	/// Gossiping is enabled when major syncing is done.
	pub fn build<
		N: NetworkPeers + NetworkEventStream,
		S: SyncEventStream + sp_consensus::SyncOracle,
	>(
		self,
		network: N,
		sync: S,
		statement_store: Arc<dyn StatementStore>,
		metrics_registry: Option<&Registry>,
		executor: impl Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send,
	) -> error::Result<StatementHandler<N, S>> {
		let sync_event_stream = sync.event_stream("statement-handler-sync");
		let (queue_sender, mut queue_receiver) = async_channel::bounded(100_000);

		let store = statement_store.clone();
		executor(
			async move {
				loop {
					let task: Option<(Statement, oneshot::Sender<SubmitResult>)> =
						queue_receiver.next().await;
					match task {
						None => return,
						Some((statement, completion)) => {
							let result = store.submit(statement, StatementSource::Network);
							if completion.send(result).is_err() {
								log::debug!(
									target: LOG_TARGET,
									"Error sending validation completion"
								);
							}
						},
					}
				}
			}
			.boxed(),
		);

		let handler = StatementHandler {
			protocol_name: self.protocol_name,
			notification_service: self.notification_service,
			propagate_timeout: (Box::pin(interval(PROPAGATE_TIMEOUT))
				as Pin<Box<dyn Stream<Item = ()> + Send>>)
				.fuse(),
			pending_statements: FuturesUnordered::new(),
			pending_statements_peers: HashMap::new(),
			network,
			sync,
			sync_event_stream: sync_event_stream.fuse(),
			peers: HashMap::new(),
			statement_store,
			queue_sender,
			metrics: if let Some(r) = metrics_registry {
				Some(Metrics::register(r)?)
			} else {
				None
			},
		};

		Ok(handler)
	}
}

/// Handler for statements. Call [`StatementHandler::run`] to start the processing.
pub struct StatementHandler<
	N: NetworkPeers + NetworkEventStream,
	S: SyncEventStream + sp_consensus::SyncOracle,
> {
	protocol_name: ProtocolName,
	/// Interval at which we call `propagate_statements`.
	propagate_timeout: stream::Fuse<Pin<Box<dyn Stream<Item = ()> + Send>>>,
	/// Pending statements verification tasks.
	pending_statements:
		FuturesUnordered<Pin<Box<dyn Future<Output = (Hash, Option<SubmitResult>)> + Send>>>,
	/// As multiple peers can send us the same statement, we group
	/// these peers using the statement hash while the statement is
	/// imported. This prevents that we import the same statement
	/// multiple times concurrently.
	pending_statements_peers: HashMap<Hash, HashSet<PeerId>>,
	/// Network service to use to send messages and manage peers.
	network: N,
	/// Syncing service.
	sync: S,
	/// Receiver for syncing-related events.
	sync_event_stream: stream::Fuse<Pin<Box<dyn Stream<Item = SyncEvent> + Send>>>,
	/// Notification service.
	notification_service: Box<dyn NotificationService>,
	// All connected peers
	peers: HashMap<PeerId, Peer>,
	statement_store: Arc<dyn StatementStore>,
	queue_sender: async_channel::Sender<(Statement, oneshot::Sender<SubmitResult>)>,
	/// Prometheus metrics.
	metrics: Option<Metrics>,
}

/// Peer information
#[derive(Debug)]
struct Peer {
	/// Holds a set of statements known to this peer.
	known_statements: LruHashSet<Hash>,
	role: ObservedRole,
}

impl<N, S> StatementHandler<N, S>
where
	N: NetworkPeers + NetworkEventStream,
	S: SyncEventStream + sp_consensus::SyncOracle,
{
	/// Turns the [`StatementHandler`] into a future that should run forever and not be
	/// interrupted.
	pub async fn run(mut self) {
		loop {
			futures::select! {
				_ = self.propagate_timeout.next() => {
					self.propagate_statements();
				},
				(hash, result) = self.pending_statements.select_next_some() => {
					if let Some(peers) = self.pending_statements_peers.remove(&hash) {
						if let Some(result) = result {
							peers.into_iter().for_each(|p| self.on_handle_statement_import(p, &result));
						}
					} else {
						log::warn!(target: LOG_TARGET, "Inconsistent state, no peers for pending statement!");
					}
				},
				sync_event = self.sync_event_stream.next() => {
					if let Some(sync_event) = sync_event {
						self.handle_sync_event(sync_event);
					} else {
						// Syncing has seemingly closed. Closing as well.
						return;
					}
				}
				event = self.notification_service.next_event().fuse() => {
					if let Some(event) = event {
						self.handle_notification_event(event)
					} else {
						// `Notifications` has seemingly closed. Closing as well.
						return
					}
				}
			}
		}
	}

	fn handle_sync_event(&mut self, event: SyncEvent) {
		match event {
			SyncEvent::InitialPeers(peer_ids) => {
				let addrs = peer_ids
					.into_iter()
					.map(|peer_id| Multiaddr::empty().with(Protocol::P2p(peer_id.into())))
					.collect();
				let result =
					self.network.add_peers_to_reserved_set(self.protocol_name.clone(), addrs);
				if let Err(err) = result {
					log::error!(target: LOG_TARGET, "Add reserved peers failed: {}", err);
				}
			},
			SyncEvent::PeerConnected(peer_id) => {
				let addr = Multiaddr::empty().with(Protocol::P2p(peer_id.into()));
				let result = self.network.add_peers_to_reserved_set(
					self.protocol_name.clone(),
					iter::once(addr).collect(),
				);
				if let Err(err) = result {
					log::error!(target: LOG_TARGET, "Add reserved peer failed: {}", err);
				}
			},
			SyncEvent::PeerDisconnected(peer_id) => {
				let result = self.network.remove_peers_from_reserved_set(
					self.protocol_name.clone(),
					iter::once(peer_id).collect(),
				);
				if let Err(err) = result {
					log::error!(target: LOG_TARGET, "Failed to remove reserved peer: {err}");
				}
			},
		}
	}

	fn handle_notification_event(&mut self, event: NotificationEvent) {
		match event {
			NotificationEvent::ValidateInboundSubstream { peer, handshake, result_tx, .. } => {
				// only accept peers whose role can be determined
				let result = self
					.network
					.peer_role(peer, handshake)
					.map_or(ValidationResult::Reject, |_| ValidationResult::Accept);
				let _ = result_tx.send(result);
			},
			NotificationEvent::NotificationStreamOpened { peer, handshake, .. } => {
				let Some(role) = self.network.peer_role(peer, handshake) else {
					log::debug!(target: LOG_TARGET, "role for {peer} couldn't be determined");
					return
				};

				let _was_in = self.peers.insert(
					peer,
					Peer {
						known_statements: LruHashSet::new(
							NonZeroUsize::new(MAX_KNOWN_STATEMENTS).expect("Constant is nonzero"),
						),
						role,
					},
				);
				debug_assert!(_was_in.is_none());
			},
			NotificationEvent::NotificationStreamClosed { peer } => {
				let _peer = self.peers.remove(&peer);
				debug_assert!(_peer.is_some());
			},
			NotificationEvent::NotificationReceived { peer, notification } => {
				// Accept statements only when node is not major syncing
				if self.sync.is_major_syncing() {
					log::trace!(
						target: LOG_TARGET,
						"{peer}: Ignoring statements while major syncing or offline"
					);
					return
				}

				if let Ok(statements) = <Statements as Decode>::decode(&mut notification.as_ref()) {
					self.on_statements(peer, statements);
				} else {
					log::debug!(target: LOG_TARGET, "Failed to decode statement list from {peer}");
				}
			},
		}
	}

	/// Called when peer sends us new statements
	fn on_statements(&mut self, who: PeerId, statements: Statements) {
		log::trace!(target: LOG_TARGET, "Received {} statements from {}", statements.len(), who);
		if let Some(ref mut peer) = self.peers.get_mut(&who) {
			for s in statements {
				if self.pending_statements.len() > MAX_PENDING_STATEMENTS {
					log::debug!(
						target: LOG_TARGET,
						"Ignoring any further statements that exceed `MAX_PENDING_STATEMENTS`({}) limit",
						MAX_PENDING_STATEMENTS,
					);
					break
				}

				let hash = s.hash();
				peer.known_statements.insert(hash);

				self.network.report_peer(who, rep::ANY_STATEMENT);

				match self.pending_statements_peers.entry(hash) {
					Entry::Vacant(entry) => {
						let (completion_sender, completion_receiver) = oneshot::channel();
						match self.queue_sender.try_send((s, completion_sender)) {
							Ok(()) => {
								self.pending_statements.push(
									async move {
										let res = completion_receiver.await;
										(hash, res.ok())
									}
									.boxed(),
								);
								entry.insert(HashSet::from_iter([who]));
							},
							Err(async_channel::TrySendError::Full(_)) => {
								log::debug!(
									target: LOG_TARGET,
									"Dropped statement because validation channel is full",
								);
							},
							Err(async_channel::TrySendError::Closed(_)) => {
								log::trace!(
									target: LOG_TARGET,
									"Dropped statement because validation channel is closed",
								);
							},
						}
					},
					Entry::Occupied(mut entry) => {
						if !entry.get_mut().insert(who) {
							// Already received this from the same peer.
							self.network.report_peer(who, rep::DUPLICATE_STATEMENT);
						}
					},
				}
			}
		}
	}

	fn on_handle_statement_import(&mut self, who: PeerId, import: &SubmitResult) {
		match import {
			SubmitResult::New(NetworkPriority::High) =>
				self.network.report_peer(who, rep::EXCELLENT_STATEMENT),
			SubmitResult::New(NetworkPriority::Low) =>
				self.network.report_peer(who, rep::GOOD_STATEMENT),
			SubmitResult::Known => self.network.report_peer(who, rep::ANY_STATEMENT_REFUND),
			SubmitResult::KnownExpired => {},
			SubmitResult::Ignored => {},
			SubmitResult::Bad(_) => self.network.report_peer(who, rep::BAD_STATEMENT),
			SubmitResult::InternalError(_) => {},
		}
	}

	/// Propagate one statement.
	pub fn propagate_statement(&mut self, hash: &Hash) {
		// Accept statements only when node is not major syncing
		if self.sync.is_major_syncing() {
			return
		}

		log::debug!(target: LOG_TARGET, "Propagating statement [{:?}]", hash);
		if let Ok(Some(statement)) = self.statement_store.statement(hash) {
			self.do_propagate_statements(&[(*hash, statement)]);
		}
	}

	fn do_propagate_statements(&mut self, statements: &[(Hash, Statement)]) {
		let mut propagated_statements = 0;

		for (who, peer) in self.peers.iter_mut() {
			// never send statements to light nodes
			if matches!(peer.role, ObservedRole::Light) {
				continue
			}

			let to_send = statements
				.iter()
				.filter_map(|(hash, stmt)| peer.known_statements.insert(*hash).then(|| stmt))
				.collect::<Vec<_>>();

			propagated_statements += to_send.len();

			if !to_send.is_empty() {
				log::trace!(target: LOG_TARGET, "Sending {} statements to {}", to_send.len(), who);
				self.notification_service.send_sync_notification(who, to_send.encode());
			}
		}

		if let Some(ref metrics) = self.metrics {
			metrics.propagated_statements.inc_by(propagated_statements as _)
		}
	}

	/// Call when we must propagate ready statements to peers.
	fn propagate_statements(&mut self) {
		// Send out statements only when node is not major syncing
		if self.sync.is_major_syncing() {
			return
		}

		log::debug!(target: LOG_TARGET, "Propagating statements");
		if let Ok(statements) = self.statement_store.statements() {
			self.do_propagate_statements(&statements);
		}
	}
}
