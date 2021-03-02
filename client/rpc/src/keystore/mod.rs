use tokio::sync::RwLock;
use url::Url;
// pub use self::gen_client::Client;
use jsonrpc_core::BoxFuture;
use jsonrpc_client_transports::transports::{http, ws};

use sp_keystore::{
    CryptoStore,
    SyncCryptoStorePtr,
    Error as TraitError,
    SyncCryptoStore,
    vrf::{VRFTranscriptData, VRFSignature, make_transcript},
};

use crate::gen_client::Client;

pub struct RemoteKeystore {
    client: RwLock<Option<Client>>,
    url: Url,
    max_retry: u8,
}

impl RemoteKeystore {
    /// Create a local keystore from filesystem.
    pub fn open(url: String, max_retry: Option<u8>) -> Result<Self> {
        let url : Url = url
            .parse()
            .map_err(|e| format!("Parsing Remote Signer URL failed: {:?}", e))?;

        match url.scheme() {
            "http" | "https" | "ws" | "wss" => {},
            _ => return Err(TraitError::Unavailable)
        };

        Ok(RemoteKeystore{
            client: RwLock::new(None),
            url,
            max_retry: max_retry.unwrap_or(10),
        })
    }

    /// Create a local keystore in memory.
    async fn ensure_connected(&self) -> Result<()> {
        let mut w = self.client.write().await;
        if w.is_some() {
            return Ok(())
        }

        log::info!{
            target: "remote_keystore" ,
            "Connecting to {:}", self.url
        };

        let mut counter = 0;
        loop {
            let client = match self.url.scheme() {
                "http" | "https" => {
                    let (sender, receiver) = futures::channel::oneshot::channel();
                    let url = self.url.clone().into_string();
                    std::thread::spawn(move || {
                        let connect = hyper::rt::lazy(move || {
                            use jsonrpc_core::futures::Future;
                            http::connect(&url)
                                .then(|client| {
                                    if sender.send(client).is_err() {
                                        panic!("The caller did not wait for the server.");
                                    }
                                    // TODO kill the tokio runtime now and the thread in case
                                    // `client.is_err()`
                                    Ok(())
                                })
                        });
                        hyper::rt::run(connect);
                    });

                    receiver.await.expect("Always sends something")
                },
                "ws" | "wss" => {
                    ws::connect::<Client>(&self.url).compat().await
                },
                _ => unreachable!()
            };

            match client {
                Ok(client) => {
                    *w = Some(client);
                    return Ok(());
                },
                Err(e) => {
                    log::warn!{
                        target: "remote_keystore",
                        "Attempt {} failed: {}", counter, e
                    }
                }
            }

            counter += 1;
            if self.max_retry > 0 && counter >= self.max_retry {
                log::error!{
                    target: "remote_keystore",
                    "Retrying to connect {:} failed {} times. Quitting.", self.url, counter
                }
                return Err(TraitError::Unavailable)
            }
        }


    }
}