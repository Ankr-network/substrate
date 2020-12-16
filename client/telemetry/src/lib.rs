// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
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

//! Telemetry utilities. TODO update doc
//!
//! You can collect telemetries for a substrate node by creating a `Telemetry` object. For every
//! substrate node there must be only one `Telemetry` object. This `Telemetry` object can connect to
//! multiple telemetry endpoints. The `Telemetry` object is a `Stream` that needs to be polled
//! regularly in order to function.
//!
//! The macro `telemetry!` can be used to report telemetries from anywhere in the code but a
//! `Telemetry` must have been initialized. Creating a `Telemetry` will make all the following code
//! execution (including newly created async background tasks) use this `Telemetry` when reporting
//! telemetries. If multiple `Telemetry` objects are created, the latest one (higher up in the
//! stack) will be used. If no `Telemetry` object can be found, nothing is reported.
//!
//! To re-use connections to the same server you need to use the `Telemetries` object to create
//! multiple `Telemetry`. `Telemetries` also manages a collection of channel `Sender` for you (see
//! `Senders`). `Telemetries` should be used unless you need finer control.
//!
//! The [`Telemetry`] struct implements `Stream` and must be polled regularly (or sent to a
//! background thread/task) in order for the telemetry to properly function.
//! The `Stream` generates [`TelemetryEvent`]s.

#![warn(missing_docs)]

use futures::{channel::mpsc, prelude::*};
use libp2p::{
	core::transport::timeout::TransportTimeout, Multiaddr, Transport,
};
use log::{error, warn};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
	collections::{HashMap, HashSet},
	io,
	pin::Pin,
	sync::Arc,
	time::Duration,
};
use wasm_timer::Instant;
use tracing::Id;
use parking_lot::Mutex;
use sp_utils::{mpsc::{tracing_unbounded, TracingUnboundedReceiver, TracingUnboundedSender}};

pub use chrono;
pub use libp2p::wasm_ext::ExtTransport;
pub use serde_json;
pub use tracing;

mod layer;
mod node;
pub mod worker;
mod dispatcher;

pub use layer::*;
use node::*;
use worker::CONNECT_TIMEOUT; // TODO mod
use dispatcher::*;

/// List of telemetry servers we want to talk to. Contains the URL of the server, and the
/// maximum verbosity level.
///
/// The URL string can be either a URL or a multiaddress.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TelemetryEndpoints(
	#[serde(deserialize_with = "url_or_multiaddr_deser")]
	Vec<(Multiaddr, u8)>
);

/// Custom deserializer for TelemetryEndpoints, used to convert urls or multiaddr to multiaddr.
fn url_or_multiaddr_deser<'de, D>(deserializer: D) -> Result<Vec<(Multiaddr, u8)>, D::Error>
	where D: Deserializer<'de>
{
	Vec::<(String, u8)>::deserialize(deserializer)?
		.iter()
		.map(|e| Ok((url_to_multiaddr(&e.0)
		.map_err(serde::de::Error::custom)?, e.1)))
		.collect()
}

impl TelemetryEndpoints {
	/// Create a `TelemetryEndpoints` based on a list of `(String, u8)`.
	pub fn new(endpoints: Vec<(String, u8)>) -> Result<Self, libp2p::multiaddr::Error> {
		let endpoints: Result<Vec<(Multiaddr, u8)>, libp2p::multiaddr::Error> = endpoints.iter()
			.map(|e| Ok((url_to_multiaddr(&e.0)?, e.1)))
			.collect();
		endpoints.map(Self)
	}
}

impl TelemetryEndpoints {
	/// Return `true` if there are no telemetry endpoints, `false` otherwise.
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

/// Parses a WebSocket URL into a libp2p `Multiaddr`.
fn url_to_multiaddr(url: &str) -> Result<Multiaddr, libp2p::multiaddr::Error> {
	// First, assume that we have a `Multiaddr`.
	let parse_error = match url.parse() {
		Ok(ma) => return Ok(ma),
		Err(err) => err,
	};

	// If not, try the `ws://path/url` format.
	if let Ok(ma) = libp2p::multiaddr::from_url(url) {
		return Ok(ma)
	}

	// If we have no clue about the format of that string, assume that we were expecting a
	// `Multiaddr`.
	Err(parse_error)
}

/// Substrate DEBUG log level.
pub const SUBSTRATE_DEBUG: u8 = 9;
/// Substrate INFO log level.
pub const SUBSTRATE_INFO: u8 = 0;

/// Consensus TRACE log level.
pub const CONSENSUS_TRACE: u8 = 9;
/// Consensus DEBUG log level.
pub const CONSENSUS_DEBUG: u8 = 5;
/// Consensus WARN log level.
pub const CONSENSUS_WARN: u8 = 4;
/// Consensus INFO log level.
pub const CONSENSUS_INFO: u8 = 1;

pub(crate) type InitPayload = (Id, TelemetryEndpoints, serde_json::Value, TelemetryConnectionSinks);

/// An object that keeps track of all the [`Telemetry`] created by its `build_telemetry()` method.
///
/// [`Telemetry`] created through this object re-use connections if possible.
#[derive(Debug)]
pub struct Telemetries {
	receiver: mpsc::Receiver<(Id, u8, String)>,
	sender: mpsc::Sender<(Id, u8, String)>,
	init_receiver: mpsc::UnboundedReceiver<InitPayload>,
	init_sender: mpsc::UnboundedSender<InitPayload>,
	transport: crate::worker::WsTrans,
}

impl Telemetries {
	/// TODO
	pub fn new() -> Result<Self, io::Error> {
		let (sender, receiver) = mpsc::channel(16);
		let (init_sender, init_receiver) = mpsc::unbounded();

		Ok(Self {
			receiver,
			sender,
			init_receiver,
			init_sender,
			transport: Self::initialize_transport()?,
		})
	}

	fn initialize_transport() -> Result<crate::worker::WsTrans, io::Error> {
		#[cfg(target_os = "unknown")]
		let transport = {
			use libp2p_wasm_ext::{ExtTransport, ffi};
			ExtTransport::new(ffi::websocket_transport())
		}.map((|inner, _| worker::StreamSink::from(inner)) as fn(_, _) -> _);

		// The main transport is the `wasm_external_transport`, but if we're on desktop we add
		// support for TCP+WebSocket+DNS as a fallback. In practice, you're not expected to pass
		// an external transport on desktop and the fallback is used all the time.
		#[cfg(not(target_os = "unknown"))]
		let transport = {
			let inner = libp2p::dns::DnsConfig::new(libp2p::tcp::TcpConfig::new())?;
			libp2p::websocket::framed::WsConfig::new(inner)
				.and_then(|connec, _| {
					let connec = connec
						.with(|item| {
							let item = libp2p::websocket::framed::OutgoingData::Binary(item);
							future::ready(Ok::<_, io::Error>(item))
						})
						.try_filter(|item| future::ready(item.is_data()))
						.map_ok(|data| data.into_bytes());
					future::ready(Ok::<_, io::Error>(connec))
				})
		};

		Ok(TransportTimeout::new(
			transport.map(|out, _| {
				let out = out
					.map_err(|err| io::Error::new(io::ErrorKind::Other, err))
					.sink_map_err(|err| io::Error::new(io::ErrorKind::Other, err));
				Box::pin(out) as Pin<Box<_>>
			}),
			CONNECT_TIMEOUT
		).boxed())
	}

	/// TODO
	pub fn handle(&self) -> TelemetryHandle {
		TelemetryHandle(self.init_sender.clone())
	}

	/// TODO
	pub fn sender(&self) -> mpsc::Sender<(Id, u8, String)> {
		self.sender.clone()
	}

	/// TODO
	pub async fn run(self) {
		let Self {
			receiver,
			sender: _sender,
			mut init_receiver,
			init_sender,
			transport,
		} = self;

		let mut node_map: HashMap<Id, Vec<(u8, Multiaddr)>> = HashMap::new();
		let mut connection_messages: HashMap<Multiaddr, Vec<serde_json::Value>> = HashMap::new();
		let mut connection_sinks: HashMap<Multiaddr, Vec<TelemetryConnectionSinks>> = HashMap::new();
		let mut existing_nodes: HashSet<Multiaddr> = HashSet::new();

		// initialize the telemetry nodes
		init_sender.close_channel();
		while let Some((id, endpoints, connection_message, connection_sink)) = init_receiver.next().await
		{
			let endpoints = endpoints.0;

			for (addr, verbosity) in endpoints {
				node_map.entry(id.clone()).or_insert_with(Vec::new)
					.push((verbosity, addr.clone()));
				existing_nodes.insert(addr.clone());
				let mut json = connection_message.clone();
				let obj = json.as_object_mut().expect("todo");
				obj.insert("msg".into(), "system.connected".into());
				obj.insert("id".into(), id.into_u64().into());
				connection_messages.entry(addr.clone()).or_insert_with(Vec::new)
					.push(json);
				connection_sinks.entry(addr.clone()).or_insert_with(Vec::new)
					.push(connection_sink.clone());
			}
		}

		let mut node_pool: Dispatcher =
			existing_nodes
				.iter()
				.map(|addr| {
					let connection_messages = connection_messages.remove(addr)
						.expect("there is a node for every connection message; qed");
					let connection_sinks = connection_sinks.remove(addr)
						.expect("there is a node for every connection sink; qed");
					let node = Node::new(transport.clone(), addr.clone(), connection_messages, connection_sinks);
					(addr.clone(), node)
				})
				.collect();

		let _ = receiver
			.filter_map(|(id, verbosity, message): (Id, u8, String)| {
				if let Some(nodes) = node_map.get(&id) {
					future::ready(Some((verbosity, message, nodes)))
				} else {
					log::error!(
						target: "telemetry",
						"Received telemetry log for unknown id ({:?}): {}",
						id,
						message,
					);
					future::ready(None)
				}
			})
			.flat_map(|(verbosity, message, nodes): (u8, String, &Vec<(u8, Multiaddr)>)| {
				let mut to_send = Vec::with_capacity(nodes.len());
				let before = Instant::now();

				for (node_max_verbosity, addr) in nodes {
					if verbosity > *node_max_verbosity {
						log::trace!(
							target: "telemetry",
							"Skipping {} for log entry with verbosity {:?}",
							addr,
							verbosity);
						continue;
					}

					to_send.push((addr.clone(), message.clone()));
				}

				if before.elapsed() > Duration::from_millis(200) {
					log::warn!(
						target: "telemetry",
						"Processing one telemetry message took more than 200ms",
					);
				}

				stream::iter(to_send)
			})
			.map(|x| Ok(x))
			.boxed()
			.forward(&mut node_pool)
			.await;

		log::error!(
			target: "telemetry",
			"Unexpected end of stream. This is a bug.",
		);
	}
}

/// TODO
#[derive(Clone, Debug)]
pub struct TelemetryHandle(mpsc::UnboundedSender<InitPayload>);

impl TelemetryHandle {
	/// Create a new [`Telemetry`] for the endpoints provided in argument.
	///
	/// The `endpoints` argument is a collection of telemetry WebSocket servers with a corresponding
	/// verbosity level. TODO doc
	pub fn start_telemetry(
		&mut self,
		endpoints: TelemetryEndpoints,
		connection_message: serde_json::Value,
	) -> TelemetryConnectionSinks {
		let connection_sink = TelemetryConnectionSinks::default();

		let span = tracing::info_span!(TELEMETRY_LOG_SPAN);
		let id = span.id().expect("the span is enabled; qed"); // TODO: this error happen if the log is disabled by the command line
		{
			let id = id.clone();
			// TODO where to drop span?
			tracing::dispatcher::get_default(move |dispatch| dispatch.enter(&id));
		}

		if let Err(err) = self.0.unbounded_send((id, endpoints, connection_message, connection_sink.clone())) {
			error!(
				target: "telemetry",
				"Could not initialize telemetry: {}",
				err,
			);
		}

		connection_sink
	}
}

// TODO maybe rename because it's confusing
/// Sinks to propagate telemetry connection established events.
#[derive(Default, Clone, Debug)]
pub struct TelemetryConnectionSinks(Arc<Mutex<Vec<TracingUnboundedSender<()>>>>);

impl TelemetryConnectionSinks {
	/// Get event stream for telemetry connection established events.
	pub fn on_connect_stream(&self) -> TracingUnboundedReceiver<()> {
		let (sink, stream) = tracing_unbounded("mpsc_telemetry_on_connect");
		self.0.lock().push(sink);
		stream
	}

	pub(crate) fn fire(&self) {
		self.0.lock().retain(|sink| {
			sink.unbounded_send(()).is_ok()
		});
	}
}

/// Translates to `tracing::info!`, but contains an additional verbosity
/// parameter which the log record is tagged with. Additionally the verbosity
/// parameter is added to the record as a key-value pair.
#[macro_export(local_inner_macros)]
macro_rules! telemetry {
	( $a:expr; $b:expr; $( $t:tt )* ) => {{
		let verbosity: u8 = $a;
		match format_fields_to_json!($($t)*) {
			Err(err) => {
				$crate::tracing::error!(
					target: "telemetry",
					"Could not serialize value for telemetry: {}",
					err,
				);
			},
			Ok(mut json) => {
				// NOTE: the span id will be added later in the JSON for the greater good
				json.insert("msg".into(), $b.into());
				json.insert("ts".into(), $crate::chrono::Local::now().to_rfc3339().into());
				let serialized_json = $crate::serde_json::to_string(&json)
					.expect("contains only string keys; qed");
				$crate::tracing::info!(target: $crate::TELEMETRY_LOG_SPAN,
					verbosity,
					json = serialized_json.as_str(),
				);
			},
		}
	}};
}

#[macro_export(local_inner_macros)]
#[doc(hidden)]
macro_rules! format_fields_to_json {
	( $k:literal => $v:expr $(,)? $(, $($t:tt)+ )? ) => {{
		$crate::serde_json::to_value(&$v)
			.map(|value| {
				let mut map = $crate::serde_json::Map::new();
				map.insert($k.into(), value);
				map
			})
			$(
				.and_then(|mut prev_map| {
					format_fields_to_json!($($t)*)
						.map(move |mut other_map| {
							prev_map.append(&mut other_map);
							prev_map
						})
				})
			)*
	}};
	( $k:literal => ? $v:expr $(,)? $(, $($t:tt)+ )? ) => {{
		let mut map = $crate::serde_json::Map::new();
		map.insert($k.into(), std::format!("{:?}", &$v).into());
		let ok_map: Result<_, $crate::serde_json::Error> = Ok(map);
		ok_map
		$(
			.and_then(|mut prev_map| {
				format_fields_to_json!($($t)*)
					.map(move |mut other_map| {
						prev_map.append(&mut other_map);
						prev_map
					})
			})
		)*
	}};
}

#[cfg(test)]
mod telemetry_endpoints_tests {
	use libp2p::Multiaddr;
	use super::TelemetryEndpoints;
	use super::url_to_multiaddr;

	#[test]
	fn valid_endpoints() {
		let endp = vec![("wss://telemetry.polkadot.io/submit/".into(), 3), ("/ip4/80.123.90.4/tcp/5432".into(), 4)];
		let telem = TelemetryEndpoints::new(endp.clone()).expect("Telemetry endpoint should be valid");
		let mut res: Vec<(Multiaddr, u8)> = vec![];
		for (a, b) in endp.iter() {
			res.push((url_to_multiaddr(a).expect("provided url should be valid"), *b))
		}
		assert_eq!(telem.0, res);
	}

	#[test]
	fn invalid_endpoints() {
		let endp = vec![("/ip4/...80.123.90.4/tcp/5432".into(), 3), ("/ip4/no:!?;rlkqre;;::::///tcp/5432".into(), 4)];
		let telem = TelemetryEndpoints::new(endp);
		assert!(telem.is_err());
	}

	#[test]
	fn valid_and_invalid_endpoints() {
		let endp = vec![("/ip4/80.123.90.4/tcp/5432".into(), 3), ("/ip4/no:!?;rlkqre;;::::///tcp/5432".into(), 4)];
		let telem = TelemetryEndpoints::new(endp);
		assert!(telem.is_err());
	}
}
