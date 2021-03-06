use futures::{StreamExt, SinkExt};
use futures::channel::mpsc::{unbounded, UnboundedSender};

use tokio_tungstenite::{WebSocketStream, MaybeTlsStream};
use tokio::net::TcpStream;
use tungstenite::protocol::Message;

pub use log::{info, debug, warn, error};
use human_panic::setup_panic;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::env;

mod local;
mod config;
mod introspect;

pub use wormhole::*;
pub use config::*;

use colour::*;
use std::time::Duration;

pub type ActiveStreams = Arc<RwLock<HashMap<StreamId, UnboundedSender<StreamMessage>>>>;

lazy_static::lazy_static! {
    pub static ref ACTIVE_STREAMS:ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
}

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}


#[tokio::main]
async fn main() {
    setup_panic!();

    let config = match Config::get() {
        Ok(config) => config,
        Err(_) => return,
    };

    e_green_ln!("Welcome to wormhole!\n{}\n", include_str!("../../wormhole_ascii.txt"));

    loop {
        let (restart_tx, mut restart_rx) = unbounded();
        let wormhole = run_wormhole(config.clone(), restart_tx);
        let _  = futures::future::select(Box::pin(wormhole), restart_rx.next()).await;
        info!("restarting wormhole");
    }
}

/// Setup the tunnel to our control server
async fn run_wormhole(config: Config, mut restart_tx: UnboundedSender<()>) {
    let websocket = connect_to_wormhole(&config).await;

    // split reading and writing
    let (mut ws_sink, mut ws_stream) = websocket.split();

    // tunnel channel
    let (mut tunnel_tx, mut tunnel_rx) = unbounded::<ControlPacket>();

    // continuously write to websocket tunnel
    tokio::spawn(async move {
        loop {
            let packet = match tunnel_rx.next().await {
                Some(data) => data,
                None => {
                    warn!("control flow didn't send anything!");
                    let _ = restart_tx.send(()).await;
                    return
                }
            };

            if let Err(e) = ws_sink.send(Message::binary(packet.serialize())).await {
                warn!("failed to write message to tunnel websocket: {:?}", e);
                let _ = restart_tx.send(()).await;
                return
            }
        }
    });

    // kick off the pings
    let _ = tunnel_tx.send(ControlPacket::Ping).await;

    // continuously read from websocket tunnel
    loop {
        match ws_stream.next().await {
            Some(Ok(message)) => {
                if let Err(e) = process_control_flow_message(&config, tunnel_tx.clone(), message.into_data()).await {
                    error!("Malformed protocol control packet: {:?}", e);
                    return
                }
            },
            Some(Err(e)) => {
                warn!("websocket read error: {:?}", e);
                return
            },
            None => {
                warn!("websocket sent none");
                return
            }
        }
    }
}

async fn connect_to_wormhole(config: &Config) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let (mut websocket, _) = tokio_tungstenite::connect_async(&config.control_url).await.expect("Failed to connect to wormhole server.");

    // send our Client Hello message
    let (client_hello, id) = ClientHello::generate(config.client_id.clone(), &config.secret_key, Some(config.sub_domain.clone()));
    info!("connecting to wormhole as client {}", &id);

    let hello = serde_json::to_vec(&client_hello).unwrap();
    websocket.send(Message::binary(hello)).await.expect("Failed to send client hello to wormhole server.");

    // wait for Server hello
    let sub_domain = match websocket.next().await.map(|d| d
        .map_err(|e| format!("websocket read error: {:?}", e) )
        .map(|m| serde_json::from_slice::<ServerHello>(&m.into_data()))
    )
    {
        Some(Ok(Ok(server_response))) => {
            match server_response {
                ServerHello::Success{ sub_domain } => {
                    info!("server accepted our connection.");
                    sub_domain
                },
                ServerHello::AuthFailed => {
                    error!("server denied our authentication token.");
                    panic!("Authentication failed. Check your authentication key.");
                },
                ServerHello::InvalidSubDomain =>{
                    panic!("Invalid sub-domain specified");
                }
                ServerHello::SubDomainInUse => {
                    error!("sub-domain already in use");
                    panic!("Cannot use this sub-domain, it's already taken.")
                }
            }
        }
        Some(Ok(Err(e))) => {
            error!("invalid server hello: {:?}", e);
            panic!("connection failed.");
        },
        Some(Err(e)) => {
            error!("websocket error: {:?}", e);
            panic!("connection failed.");
        }
        None => {
            panic!("Empty reply from server. Unknown failure to connect to server.")
        }
    };

    eprintln!("Wormhole activated on: {}", config.activation_url(&sub_domain));
    websocket
}

async fn process_control_flow_message(config: &Config, mut tunnel_tx: UnboundedSender<ControlPacket>, payload: Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
    let control_packet = ControlPacket::deserialize(&payload)?;

    match control_packet {
        ControlPacket::Init(stream_id) => {
            info!("stream[{:?}] -> init", stream_id.to_string());
        },
        ControlPacket::Ping => {
            log::info!("got ping");

            let mut tx = tunnel_tx.clone();
            tokio::spawn(async move {
                tokio::time::delay_for(Duration::new(PING_INTERVAL, 0)).await;
                let _ = tx.send(ControlPacket::Ping).await;
            });
        },
        ControlPacket::Refused(_) => {
            return Err("unexpected control packet".into())
        }
        ControlPacket::End(stream_id) => {
            // find the stream
            info!("got end stream [{:?}]", &stream_id);
            tokio::spawn(async move {
                let stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();
                if let Some(mut tx) = stream {
                    tokio::time::delay_for(Duration::from_secs(5)).await;
                    let _ = tx.send(StreamMessage::Close).await.map_err(|e| {
                        error!("failed to send stream close: {:?}", e);
                    });
                    ACTIVE_STREAMS.write().unwrap().remove(&stream_id);
                }
            });

        },
        ControlPacket::Data(stream_id, data) => {
            info!("stream[{:?}] -> new data: {:?}", stream_id.to_string(), data.len());

            if !ACTIVE_STREAMS.read().unwrap().contains_key(&stream_id) {
                local::setup_new_stream(&config.local_port, tunnel_tx.clone(), stream_id.clone()).await;
            }

            // find the right stream
            let active_stream = ACTIVE_STREAMS.read().unwrap().get(&stream_id).cloned();

            // forward data to it
            if let Some(mut tx) = active_stream {
                tx.send(StreamMessage::Data(data)).await?;
                info!("forwarded to local tcp ({})", stream_id.to_string());
            } else {
                error!("got data but no stream to send it to.");
                let _ = tunnel_tx.send(ControlPacket::Refused(stream_id)).await?;
            }
        },
    };

    Ok(())
}