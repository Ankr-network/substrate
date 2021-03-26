// This file is part of Substrate.

// Copyright (C) 2020 Parity Technologies (UK) Ltd.
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

use std::{
	pin::Pin,
	task::{Context, Poll}
};
use sp_core::{
	Bytes,
	crypto::{
		key_types,
		CryptoTypePublicPair,
		KeyTypeId,
	},
	ed25519,
	ecdsa,
	sr25519,
};
use sp_keystore::{
	CryptoStore, Error as CryptoStoreError,
	vrf::VRFTranscriptData,
};

use jsonrpc_core::{BoxFuture, Error as RpcError};
use serde_json::{Value};

use futures::{
	channel::{
		oneshot,
		mpsc::{UnboundedSender, UnboundedReceiver, unbounded},
	},
	future::{Future, FutureExt, TryFutureExt},
	stream:: Stream,
};
use std::convert::TryInto;

use sp_consensus_babe::BABE_ENGINE_ID;

use tonic::{transport::Server, Request, Response, Status};

use blockchain_signer::blockchain_signer_server::{BlockchainSigner, BlockchainSignerServer};
use blockchain_signer::{GetValidatingKeysRequest, GetValidatingKeysReply, SignDataRequest, SignDataReply,
						SignTransactionRequest, SignTransactionReply, SignVrfRequest, SignVrfReply, BlockchainType};

pub mod blockchain_signer {
	tonic::include_proto!("com.ankr.staking"); // The string specified here must match the proto package name
}

use crate::TransferableVRFTranscriptData;
use jsonrpc_core::futures::future::result;

impl TryInto<VRFTranscriptData> for TransferableVRFTranscriptData {
	type Error = String;

	fn try_into(self: TransferableVRFTranscriptData) -> Result<VRFTranscriptData, Self::Error> {

		let (label, field_names) = {
			if self.label.as_slice() == &BABE_ENGINE_ID {
				(&BABE_ENGINE_ID, vec!["slot number", "current epoch", "chain randomness"])
			} else {
				return Err(format!("VRF Label '{:?}' not supported", self.label))
			}
		};

		if field_names.len() != self.items.len() {
			return Err(format!("Expected '{:?}' to have {:} items but found {:}",
				label, field_names.len(), self.items.len()))
		}

		Ok(VRFTranscriptData {
			label, items: field_names.into_iter().zip(self.items).collect::<Vec<_>>()
		})
	}
}

enum State<Store: CryptoStore> {
	Idle(Store),
	Pending(Pin<Box<dyn Future<Output = Store> + Send>>),
	Ended,
}

/// Wrapping the internal Async CryptoStore
pub struct KeystoreReceiver<Store: CryptoStore> {
	receiver: UnboundedReceiver<KeystoreRequest>,
	state: State<Store>,
}

impl<Store: CryptoStore> Unpin for KeystoreReceiver<Store> { }

impl<Store: CryptoStore + 'static> KeystoreReceiver<Store> {
	fn new(store: Store, receiver: UnboundedReceiver<KeystoreRequest>) -> Self {
		KeystoreReceiver {
			receiver,
			state: State::Idle(store),
		}
	}

	fn process_request(store: Store, request: KeystoreRequest) -> Pin<Box<dyn Future<Output = Store> + Send>> {
		let sender = request.sender;
		match request.method {
			RequestMethod::Sr25519PublicKeys(id) => {
				Box::pin(async move {
					let result = store.sr25519_public_keys(id).await;
					println!("print {:?}",result.clone());
					let _ = sender.send(KeystoreResponse::Sr25519PublicKeys(result));
					return store;
				})
			},
			RequestMethod::Sr25519VrfSign(id, public, data) => {
				Box::pin(async move {
					let result = store.sr25519_vrf_sign(id, &public, data).await;
					let _ = sender.send(KeystoreResponse::Sr25519VrfSign(result));
					return store;
				})
			},
			RequestMethod::Sr25519GenerateNew(id, seed) => {
				Box::pin(async move {
					let result = store.sr25519_generate_new(id, seed.as_deref()).await;
					let _ = sender.send(KeystoreResponse::Sr25519GenerateNew(result));
					return store;
				})
			},
			RequestMethod::Ed25519PublicKeys(id) => {
				Box::pin(async move {
					let result = store.ed25519_public_keys(id).await;
					let _ = sender.send(KeystoreResponse::Ed25519PublicKeys(result));
					return store;
				})
			},
			RequestMethod::Ed25519GenerateNew(id, seed) => {
				Box::pin(async move {
					let result = store.ed25519_generate_new(id, seed.as_deref()).await;
					let _ = sender.send(KeystoreResponse::Ed25519GenerateNew(result));
					return store;
				})
			},
			RequestMethod::EcdsaPublicKeys(id) => {
				Box::pin(async move {
					let result = store.ecdsa_public_keys(id).await;
					let _ = sender.send(KeystoreResponse::EcdsaPublicKeys(result));
					return store;
				})
			},
			RequestMethod::EcdsaGenerateNew(id, seed) => {
				Box::pin(async move {
					let result = store.ecdsa_generate_new(id, seed.as_deref()).await;
					let _ = sender.send(KeystoreResponse::EcdsaGenerateNew(result));
					return store;
				})
			},
			RequestMethod::HasKeys(keys) => {
				Box::pin(async move {
					let result = store.has_keys(&keys).await;
					let _ = sender.send(KeystoreResponse::HasKeys(result));
					return store;
				})
			},
			RequestMethod::SupportedKeys(id, keys) => {
				Box::pin(async move {
					let result = store.supported_keys(id, keys).await;
					let _ = sender.send(KeystoreResponse::SupportedKeys(result));
					return store;
				})
			},
			RequestMethod::Keys(id) => {
				Box::pin(async move {
					let result = store.keys(id).await;
					let _ = sender.send(KeystoreResponse::Keys(result));
					return store;
				})
			},
			RequestMethod::InsertUnknown(key_type, suri, pubkey) => {
				Box::pin(async move {
					let result = store.insert_unknown(
						key_type,
						suri.as_str(),
						&pubkey,
					).await;
					let _ = sender.send(KeystoreResponse::InsertUnknown(result));
					return store;
				})
			},
			RequestMethod::SignWith(id, key, msg) => {
				Box::pin(async move {
					let result = store.sign_with(id, &key, &msg).await;
					let _ = sender.send(KeystoreResponse::SignWith(result));
					return store;
				})
			},
			RequestMethod::SignWithAny(id, keys, msg) => {
				Box::pin(async move {
					let result = store.sign_with_any(id, keys, &msg).await;
					let _ = sender.send(KeystoreResponse::SignWithAny(result));
					return store;
				})
			},
			RequestMethod::SignWithAll(id, keys, msg) => {
				Box::pin(async move {
					let result = store.sign_with_all(id, keys, &msg).await;
					let _ = sender.send(KeystoreResponse::SignWithAll(result));
					return store;
				})
			},
		}
	}
}

impl<Store: CryptoStore + 'static> Stream for KeystoreReceiver<Store> {
	type Item = ();

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
		let this = &mut *self;
		loop {
			match std::mem::replace(&mut this.state, State::Ended) {
				State::Idle(store) => {
					match Pin::new(&mut this.receiver).poll_next(cx) {
						Poll::Ready(None) => {
							return Poll::Ready(Some(()));
						},
						Poll::Ready(Some(request)) => {
							let future = KeystoreReceiver::process_request(store, request);
							this.state = State::Pending(future);
						},
						Poll::Pending => {
							this.state = State::Idle(store);
							return Poll::Pending;
						}
					}
				},
				State::Pending(mut future) => {
					match future.poll_unpin(cx) {
						Poll::Ready(store) => {
							this.state = State::Idle(store);
						},
						Poll::Pending => {
							this.state = State::Pending(future);
							return Poll::Pending;
						}
					}
				},
				State::Ended => {
					return Poll::Ready(None);
				}
			}
		}
	}
}


enum RequestMethod {
	Sr25519PublicKeys(KeyTypeId),
	Sr25519GenerateNew(KeyTypeId, Option<String>),
	Sr25519VrfSign(
		KeyTypeId,
		sp_application_crypto::sr25519::Public,
		sp_keystore::vrf::VRFTranscriptData,
	),
	Ed25519PublicKeys(KeyTypeId),
	Ed25519GenerateNew(KeyTypeId, Option<String>),
	EcdsaPublicKeys(KeyTypeId),
	EcdsaGenerateNew(KeyTypeId, Option<String>),
	InsertUnknown(KeyTypeId, String, Vec<u8>),
	SupportedKeys(KeyTypeId, Vec<CryptoTypePublicPair>),
	Keys(KeyTypeId,),
	HasKeys(Vec<(Vec<u8>, KeyTypeId)>),
	SignWith(KeyTypeId, CryptoTypePublicPair, Vec<u8>),
	SignWithAny(KeyTypeId, Vec<CryptoTypePublicPair>, Vec<u8>),
	SignWithAll(KeyTypeId, Vec<CryptoTypePublicPair>, Vec<u8>),
}

struct KeystoreRequest {
	sender: oneshot::Sender<KeystoreResponse>,
	method: RequestMethod,
}

enum KeystoreResponse {
	Sr25519PublicKeys(Vec<sr25519::Public>),
	Sr25519GenerateNew(
		Result<sp_application_crypto::sr25519::Public, CryptoStoreError>
	),
	Sr25519VrfSign(
		Result<sp_keystore::vrf::VRFSignature, CryptoStoreError>
	),
	Ed25519PublicKeys(Vec<ed25519::Public>),
	Ed25519GenerateNew(
		Result<sp_application_crypto::ed25519::Public, CryptoStoreError>
	),
	EcdsaPublicKeys(Vec<ecdsa::Public>),
	EcdsaGenerateNew(
		Result<sp_application_crypto::ecdsa::Public, CryptoStoreError>
	),
	InsertUnknown(Result<(), ()>),
	SupportedKeys(Result<Vec<CryptoTypePublicPair>, CryptoStoreError>),
	Keys(Result<Vec<CryptoTypePublicPair>, CryptoStoreError>),
	HasKeys(bool),
	SignWith(Result<Vec<u8>, CryptoStoreError>),
	SignWithAny(Result<(CryptoTypePublicPair, Vec<u8>), CryptoStoreError>),
	SignWithAll(Result<Vec<Result<Vec<u8>, CryptoStoreError>>, ()>),
}

pub struct RemoteSignerServer {
	sender: UnboundedSender<KeystoreRequest>,
}

impl RemoteSignerServer {

	pub fn proxy<Store: CryptoStore + 'static>(store: Store) -> (Self, KeystoreReceiver<Store>) {
		let (sender, receiver) = unbounded::<KeystoreRequest>();
		(RemoteSignerServer { sender }, KeystoreReceiver::new(store, receiver))
	}


	fn send_request(
		&self,
		request: RequestMethod
	) ->  oneshot::Receiver<KeystoreResponse> {
		let (request_sender, receiver) = oneshot::channel::<KeystoreResponse>();

		let request = KeystoreRequest {
			sender: request_sender,
			method: request,
		};
		self.sender.unbounded_send(request).expect("Unbounded Send doesn't fail");
		receiver
	}
}

#[tonic::async_trait]
impl BlockchainSigner for RemoteSignerServer {

	async fn get_validating_keys(
		&self,
		request: Request<GetValidatingKeysRequest>,
	) -> Result<Response<GetValidatingKeysReply>, Status> {
		// println!("Got a request: {:?}", request.into_inner().id);

		let mut validating_key:Vec<_> = Vec::new();

		let mut id:KeyTypeId = key_types::AURA;

		match request.into_inner().id {
			1 => id = key_types::BABE,
			_ => ()
		}

		let receiver = self.send_request(RequestMethod::Sr25519PublicKeys(id)).await.unwrap();

		match receiver {
			KeystoreResponse::Sr25519PublicKeys(key) => {
				// println!("{:?}", key[0].to_vec());
				for (pos, e) in key.iter().enumerate() {
					validating_key.push(blockchain_signer::BlockchainValidatingKey { r#type: 2, public_key: e.to_vec(), crypto: 2 });
				}
			},
			_ => (),
		};

		let reply = blockchain_signer::GetValidatingKeysReply {
			public_keys: validating_key,
		};

		Ok(Response::new(reply))
	}

	async fn sign_data(
		&self,
		request: Request<SignDataRequest>,
	) -> Result<Response<SignDataReply>, Status> {
		// println!("Got a request: {:?}", request.into_inner().public_key.unwrap().public_key);

		//fails if blockchain != Polkadot
		assert_eq!(request.into_inner().public_key.unwrap().r#type, 2);

		// let receiver = self.send_request(RequestMethod::SignWith(id, key, msg)).await.unwrap();
		//
		// match receiver {
		// 	KeystoreResponse::SignWith(result) => {
		// 		let ret:Vec<u8> = result;
		// 		Ok(ret);
		// 	}
		// 	_ => (),
		// }

		// Box::new(self.send_request(RequestMethod::SignWith(id, key, msg)).map(|response|
		// 	if let KeystoreResponse::SignWith(result) =  response {
		// 		result.map_err(|_| RpcError::internal_error())
		// 	} else {
		// 		Err(RpcError::internal_error())
		// 	}
		// ).boxed().compat());

		// tests

		let pkey = vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

		// let sign = vec![blockchain_signer::BlockchainValidatingKey { r#type: 2, public_key: pkey, crypto: 2 }];

		// let message = vec![0u8, 1, 2, 3];
		// let raw_payload = vec![0u8, 4, 5, 6];
		// let mime_type = vec![0u8, 7, 8, 9];

		// let public_key: Bytes = 0x143fa4ecea108937a2324d36ee4cbce3c6f3a08b0499b276cd7adb7a7631a559;
		// let key_type: String = "aura".to_string();

		//["0x143fa4ecea108937a2324d36ee4cbce3c6f3a08b0499b276cd7adb7a7631a559","aura"];

		// let key_type = key_type.as_str().try_into().map_err(|_| Error::BadKeyType)?;
		// Ok(SyncCryptoStore::has_keys(&*self.keystore, &[(public_key.to_vec(), key_type)]));

		let reply = blockchain_signer::SignDataReply {
			signature: pkey,
		};

		Ok(Response::new(reply))
	}

	async fn sign_transaction(
		&self,
		request: Request<SignTransactionRequest>,
	) -> Result<Response<SignTransactionReply>, Status> {
		println!("Got a request");

		let reply = blockchain_signer::SignTransactionReply {};

		Ok(Response::new(reply))
	}

	async fn sign_vrf(
		&self,
		request: Request<SignVrfRequest>,
	) -> Result<Response<SignVrfReply>, Status> {
		println!("Got a request");

		let reply = blockchain_signer::SignVrfReply {};

		Ok(Response::new(reply))
	}

	// ToDo: remove old rpc functions
	// fn sr25519_public_keys(&self, id: KeyTypeId) -> BoxFuture<Vec<sr25519::Public>> {
	// 	let receiver = self.send_request(RequestMethod::Sr25519PublicKeys(id));
	// 	Box::new(receiver.map(|e| match e {
	// 		Ok(KeystoreResponse::Sr25519PublicKeys(keys)) => Ok(keys),
	// 		_ => Ok(vec![]),
	// 	}).boxed().compat())
	// }
	//
	//
    // fn sr25519_generate_new(
	// 	&self,
	// 	id: KeyTypeId,
	// 	seed: Option<String>,
	// ) -> BoxFuture<sp_application_crypto::sr25519::Public> {
	// 	Box::new(self.send_request(
	// 		RequestMethod::Sr25519GenerateNew(id, seed)
	// 	).map(|response|
	// 		if  let Ok(KeystoreResponse::Sr25519GenerateNew(result)) = response {
	// 			 result.map_err(|_|RpcError::internal_error())
	// 		} else {
	// 			Err(RpcError::internal_error())
	// 		}
	// 	).boxed().compat())
    // }
	//
	// fn ed25519_public_keys(&self, id: KeyTypeId)
	// 	-> BoxFuture<Vec<sp_application_crypto::ed25519::Public>>
	// {
	// 	Box::new(self.send_request(RequestMethod::Ed25519PublicKeys(id)).map(|response|
	// 		if let Ok(KeystoreResponse::Ed25519PublicKeys(keys)) = response {
	// 			Ok(keys)
	// 		} else {
	// 			Ok(vec![])
	// 		}
	// 	).boxed().compat())
    // }
	//
    // fn ed25519_generate_new(
	// 	&self,
	// 	id: KeyTypeId,
	// 	seed: Option<String>,
	// ) -> BoxFuture<sp_application_crypto::ed25519::Public> {
	// 	Box::new(self.send_request(
	// 		RequestMethod::Ed25519GenerateNew(id, seed)
	// 	).map(|response|
	// 		if let Ok(KeystoreResponse::Ed25519GenerateNew(result)) = response {
	// 			result.map_err(|_| RpcError::internal_error())
	// 		} else {
	// 			Err(RpcError::internal_error())
	// 		}
	// 	).boxed().compat())
    // }
	//
	// fn ecdsa_public_keys(&self, id: KeyTypeId)
	// 	-> BoxFuture<Vec<sp_application_crypto::ecdsa::Public>>
	// {
	// 	Box::new(self.send_request(RequestMethod::EcdsaPublicKeys(id)).map(|response|
	// 		if let Ok(KeystoreResponse::EcdsaPublicKeys(keys)) = response
	// 		{
	// 			Ok(keys)
	// 		} else {
	// 			Ok(vec![])
	// 		}
	// 	).boxed().compat())
    // }
	//
    // fn ecdsa_generate_new(
	// 	&self,
	// 	id: KeyTypeId,
	// 	seed: Option<String>,
	// ) -> BoxFuture<sp_application_crypto::ecdsa::Public> {
	// 	Box::new(self.send_request(
	// 		RequestMethod::EcdsaGenerateNew(id, seed)
	// 	).map(|response|
	// 		if let Ok(KeystoreResponse::EcdsaGenerateNew(result)) = response
	// 			 {
	// 			result.map_err(|_| RpcError::internal_error())
	// 		} else {
	// 			Err(RpcError::internal_error())
	// 		}
	// 	).boxed().compat())
    // }
	//
    // fn insert_unknown(&self, key_type: KeyTypeId, suri: String, public: Vec<u8>) -> BoxFuture<()> {
	// 	Box::new(
	// 		self.send_request(RequestMethod::InsertUnknown(
	// 				key_type, suri, public)
	// 		).map(|_| Ok(())).boxed().compat())
	// }
	//
    // fn supported_keys(
	// 	&self,
	// 	id: KeyTypeId,
	// 	keys: Vec<CryptoTypePublicPair>
	// ) -> BoxFuture<Vec<CryptoTypePublicPair>> {
	// 	Box::new(self.send_request(RequestMethod::SupportedKeys(id, keys)).map(|response|
	// 		if let Ok(KeystoreResponse::SupportedKeys(keys)) = response {
	// 			keys.map_err(|_| RpcError::internal_error())
	// 		} else {
	// 			Ok(vec![])
	// 		}
	// 	).boxed().compat())
    // }
	//
    // fn keys(&self, id: KeyTypeId) -> BoxFuture<Vec<CryptoTypePublicPair>> {
	// 	Box::new(self.send_request(RequestMethod::Keys(id)).map(|response|
	// 		if let Ok(KeystoreResponse::Keys(keys)) = response {
	// 			keys.map_err(|_| RpcError::internal_error())
	// 		} else {
	// 			Ok(vec![])
	// 		}
	// 	).boxed().compat())
    // }
	//
    // fn has_keys(&self, public_keys: Vec<(Vec<u8>, KeyTypeId)>) -> BoxFuture<bool> {
	// 	Box::new(self.send_request(RequestMethod::HasKeys(public_keys.to_vec())).map(|response|
	// 		if let Ok(KeystoreResponse::HasKeys(exists)) = response {
	// 			Ok(exists)
	// 		} else {
	// 			Ok(false)
	// 		}
	// 	).boxed().compat())
    // }
	//
    // fn sign_with(
	// 	&self,
	// 	id: KeyTypeId,
	// 	key: CryptoTypePublicPair,
	// 	msg: Vec<u8>,
	// ) -> BoxFuture<Vec<u8>> {
	// 	Box::new(self.send_request(RequestMethod::SignWith(id, key, msg)).map(|response|
	// 		if let Ok(KeystoreResponse::SignWith(result)) =  response {
	// 			result.map_err(|_| RpcError::internal_error())
	// 		} else {
	// 			Err(RpcError::internal_error())
	// 		}
	// 	).boxed().compat())
	// }
	//
	// fn sign_with_any(
	// 	&self,
	// 	id: KeyTypeId,
	// 	keys: Vec<CryptoTypePublicPair>,
	// 	msg: Vec<u8>
	// ) -> BoxFuture<(CryptoTypePublicPair, Vec<u8>)> {
	// 	Box::new(self.send_request(RequestMethod::SignWithAny(id, keys, msg)).map(|response|
	// 		if let Ok(KeystoreResponse::SignWithAny(result)) =  response {
	// 			result.map_err(|_| RpcError::internal_error())
	// 		} else {
	// 			Err(RpcError::internal_error())
	// 		}
	// 	).boxed().compat())
	// }
	//
	// fn sign_with_all(
	// 	&self,
	// 	id: KeyTypeId,
	// 	keys: Vec<CryptoTypePublicPair>,
	// 	msg: Vec<u8>,
	// ) -> BoxFuture<Vec<Result<Vec<u8>, String>>> {
	// 	Box::new(self.send_request(RequestMethod::SignWithAll(id, keys, msg)).map(|response|
	// 		if let Ok(KeystoreResponse::SignWithAll(result)) =  response {
	// 			result.map_err(|_| RpcError::internal_error())
	// 			.map(|v| v.into_iter().map(|i| i.map_err(|e| e.to_string())).collect())
	// 		} else {
	// 			Err(RpcError::internal_error())
	// 		}
	// 	).boxed().compat())
	// }
	//
    // fn sr25519_vrf_sign(
	// 	&self,
	// 	key_type: KeyTypeId,
	// 	public: sp_application_crypto::sr25519::Public,
	// 	transcript_data: crate::TransferableVRFTranscriptData,
	// ) -> BoxFuture<sp_keystore::vrf::VRFSignature> {
	//
	// 	match transcript_data.try_into() {
	// 		Ok(vrf_data) =>  Box::new(
	// 			self.send_request(RequestMethod::Sr25519VrfSign(key_type, public, vrf_data))
	// 				.map(|response|
	// 					if let Ok(KeystoreResponse::Sr25519VrfSign(result)) = response {
	// 						result.map_err(|_| RpcError::internal_error())
	// 					} else {
	// 						Err(RpcError::internal_error())
	// 					}
	// 				).boxed()
	// 			.compat()
	// 		),
	// 		Err(e) => Box::new(futures::future::err(RpcError::invalid_params(e)).compat())
	// 	}
    // }
}

// #[cfg(test)]
// mod tests {
// 	use tokio;
// 	use serde_json;
// 	use futures::StreamExt;
// 	use sp_keystore::CryptoStore;
// 	use jsonrpc_test;
// 	use sc_keystore::LocalKeystore;
//
// 	use super::*;
// 	use crate::RemoteSignerApi;
//
// 	const TEST_TK : KeyTypeId = KeyTypeId(*b"test");
// 	const TEST_TK_NOPE : KeyTypeId = KeyTypeId(*b"nope");
//
// 	async fn setup(msg_count: u8) -> jsonrpc_test::Rpc {
// 		let keystore = LocalKeystore::in_memory();
// 		keystore.sr25519_generate_new(TEST_TK, Some("//Alice"))
// 			.await.expect("InMem Keystore doesn't fail");
// 		keystore.ed25519_generate_new(TEST_TK, Some("//Bob"))
// 			.await.expect("InMem Keystore doesn't fail");
// 		keystore.ed25519_generate_new(TEST_TK, Some("//Charlie"))
// 			.await.expect("InMem Keystore doesn't fail");
//
// 		{
// 			use sp_keystore::SyncCryptoStore;
// 			assert_eq!(SyncCryptoStore::sr25519_public_keys(&keystore, TEST_TK).len(), 3);
// 		}
//
// 		let (server, mut runner) = RemoteSignerServer::proxy(keystore);
//
// 		tokio::task::spawn(async move {
// 			for _ in 0..msg_count {
// 				runner.next().await;
// 			}
// 		});
//
// 		jsonrpc_test::Rpc::new(RemoteSignerApi::to_delegate(server))
// 	}
//
// 	#[tokio::test(core_threads=4)]
// 	async fn test_keys() {
// 		let rpc = setup(2).await;
// 		let r = rpc.request("signer_keys", &[TEST_TK]);
// 		let res : Vec<CryptoTypePublicPair> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 9);
//
// 		let r = rpc.request("signer_keys", &[TEST_TK_NOPE]);
// 		let res : Vec<CryptoTypePublicPair> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 0);
// 	}
//
// 	#[tokio::test(core_threads=4)]
// 	async fn test_sr25519_public_keys() {
// 		let rpc = setup(2).await;
// 		let r = rpc.request("signer_sr25519_public_keys", &[TEST_TK]);
// 		let res : Vec<sr25519::Public> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 3);
//
// 		let r = rpc.request("signer_sr25519_public_keys", &[TEST_TK_NOPE]);
// 		let res : Vec<sr25519::Public> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 0);
// 	}
//
// 	#[tokio::test(core_threads=4)]
// 	async fn test_ed25519_public_keys() {
// 		let rpc = setup(2).await;
// 		let r = rpc.request("signer_ed25519_public_keys", &[TEST_TK]);
// 		let res : Vec<ed25519::Public> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 3);
//
// 		let r = rpc.request("signer_ed25519_public_keys", &[TEST_TK_NOPE]);
// 		let res : Vec<ed25519::Public> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 0);
// 	}
//
// 	#[tokio::test(core_threads=4)]
// 	#[ignore]
// 	async fn test_ecdsa_public_keys() {
// 		let rpc = setup(2).await;
// 		let r = rpc.request("signer_ecdsa_public_keys", &[TEST_TK]);
// 		let res : Vec<ecdsa::Public> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 2);
//
// 		let r = rpc.request("signer_ecdsa_public_keys", &[TEST_TK_NOPE]);
// 		let res : Vec<ecdsa::Public> = serde_json::from_str(&r).unwrap();
// 		assert_eq!(res.len(), 0);
// 	}
// }