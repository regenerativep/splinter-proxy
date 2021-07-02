use std::{
    net::SocketAddr,
    sync::Arc,
};

use craftio_rs::{
    CraftAsyncReader,
    CraftAsyncWriter,
    CraftIo,
};
use mcproto_rs::{
    protocol::{
        HasPacketKind,
        State,
    },
    types::Chat,
    v1_16_3::{
        ChatPosition,
        HandshakeNextState,
        Packet753,
        PlayDisconnectSpec,
        PlayServerChatMessageSpec,
        RawPacket753,
        StatusPongSpec,
        StatusRequestSpec,
        StatusResponseSpec,
    },
};
use smol::lock::Mutex;

use super::{
    version::V753,
    AsyncCraftConnection,
    AsyncCraftReader,
    AsyncCraftWriter,
    PacketDestination,
    PacketSender,
};
use crate::{
    chat::ToChat,
    client::{
        ClientVersion,
        SplinterClient,
    },
    commands::CommandSender,
    events::LazyDeserializedPacket,
    mapping::SplinterMapping,
    protocol::{
        version,
        ProtocolVersion,
    },
    proxy::{
        ClientKickReason,
        SplinterProxy,
    },
    server::SplinterServerConnection,
};

mod chat;
mod eid;
mod login;
mod tags;
mod uuid;
pub use chat::*;
pub use eid::*;
pub use login::*;
pub use tags::*;
pub use uuid::*;

pub async fn handle_client_status(
    mut conn: AsyncCraftConnection,
    addr: SocketAddr,
    proxy: Arc<SplinterProxy>,
) -> anyhow::Result<()> {
    conn.set_state(State::Status);
    conn.write_packet_async(Packet753::StatusResponse(StatusResponseSpec {
        response: proxy.config.server_status(&*proxy),
    }))
    .await?;
    loop {
        match conn.read_packet_async::<RawPacket753>().await? {
            Some(Packet753::StatusPing(body)) => {
                conn.write_packet_async(Packet753::StatusPong(StatusPongSpec {
                    payload: body.payload,
                }))
                .await?;
                break;
            }
            Some(Packet753::StatusRequest(StatusRequestSpec)) => {
                // do nothing.
                // notchian client does not like it when we respond
                // with a server status to this message
            }
            Some(other) => error!("Unexpected packet {:?} from {}", other, addr),
            None => break,
        }
    }
    Ok(())
}

type RelayPassFn = Box<
    dyn Send
        + Sync
        + Fn(
            &Arc<SplinterProxy>,
            &PacketSender,
            &mut LazyDeserializedPacket<V753>,
            &mut SplinterMapping,
            &mut PacketDestination,
        ),
>;
pub struct RelayPass(pub RelayPassFn);

inventory::collect!(RelayPass);

pub async fn handle_server_relay(
    proxy: Arc<SplinterProxy>,
    client: Arc<SplinterClient>,
    server_conn: Arc<Mutex<SplinterServerConnection>>,
    mut server_reader: AsyncCraftReader,
) -> anyhow::Result<()> {
    let server = Arc::clone(&server_conn.lock().await.server);
    let sender = PacketSender::Server(&server);
    loop {
        // server->proxy->client
        if !**client.alive.load() || !server_conn.lock().await.alive {
            break;
        }
        let packet_opt = match server_reader.read_raw_packet_async::<RawPacket753>().await {
            Ok(packet) => packet,
            Err(e) => {
                error!("Failed to read packet from server {}: {}", server.id, e);
                continue;
            }
        };
        match packet_opt {
            Some(raw_packet) => {
                let mut lazy_packet =
                    LazyDeserializedPacket::<version::V753>::from_raw_packet(raw_packet);
                let map = &mut *proxy.mapping.lock().await;
                let mut destination = PacketDestination::Client;
                for pass in inventory::iter::<RelayPass> {
                    (pass.0)(&proxy, &sender, &mut lazy_packet, map, &mut destination);
                }
                // let writer = &mut *client.writer.lock().await;
                if let Err(e) = send_packet(&client, &destination, lazy_packet).await {
                    error!("Sending packet from server {} failure: {}", server.id, e);
                }
            }
            None => {
                // connection closed
                break;
            }
        }
    }
    server_conn.lock().await.alive = false;
    info!(
        "Server connection between {} and server id {} closed",
        client.name, server.id
    );
    Ok(())
}

async fn send_packet(
    client: &Arc<SplinterClient>,
    destination: &PacketDestination,
    lazy_packet: LazyDeserializedPacket<'_, V753>,
) -> anyhow::Result<()> {
    match destination {
        PacketDestination::Client => {
            let writer = &mut *client.writer.lock().await;
            if let Err(e) = write_packet(writer, lazy_packet).await {
                bail!(
                    "Failed to write packet to client \"{}\": {}",
                    &client.name,
                    e
                );
            }
        }
        PacketDestination::Server(server_id) => {
            let servers = client.servers.lock().await;
            let mut server_conn = servers
                .get(server_id)
                .ok_or_else(|| {
                    anyhow!(
                        "Tried to redirect packet to unknown server id \"{}\"",
                        server_id
                    )
                })?
                .lock()
                .await;
            let writer = &mut server_conn.writer;
            if let Err(e) = write_packet(writer, lazy_packet).await {
                bail!(
                    "Failed to write packet to server \"{}\": {}",
                    server_conn.server.id,
                    e
                );
            }
        }
        PacketDestination::None => return Ok(()),
    };
    Ok(())
}

async fn write_packet(
    writer: &mut AsyncCraftWriter,
    lazy_packet: LazyDeserializedPacket<'_, V753>,
) -> anyhow::Result<()> {
    if lazy_packet.is_deserialized() {
        writer
            .write_packet_async(lazy_packet.into_packet()?)
            .await?;
    } else {
        writer
            .write_raw_packet_async(lazy_packet.into_raw_packet().unwrap())
            .await?;
    }
    Ok(())
}

pub async fn handle_client_relay(
    proxy: Arc<SplinterProxy>,
    client: Arc<SplinterClient>,
    mut client_reader: AsyncCraftReader,
    client_addr: SocketAddr,
) -> anyhow::Result<()> {
    let client_arc_clone = Arc::clone(&client);
    let sender = PacketSender::Proxy(&client_arc_clone);
    loop {
        // client->proxy->server
        if !**client.alive.load() {
            break;
        }
        let packet_opt = match client_reader.read_raw_packet_async::<RawPacket753>().await {
            Ok(packet) => packet,
            Err(e) => {
                error!(
                    "Failed to read packet from {}, {}: {}",
                    client.name, client_addr, e
                );
                continue;
            }
        };
        match packet_opt {
            Some(raw_packet) => {
                let mut lazy_packet =
                    LazyDeserializedPacket::<version::V753>::from_raw_packet(raw_packet);
                let map = &mut *proxy.mapping.lock().await;
                let mut destination = PacketDestination::Server(**client.active_server_id.load());
                for pass in inventory::iter::<RelayPass> {
                    (pass.0)(&proxy, &sender, &mut lazy_packet, map, &mut destination);
                }
                if let Err(e) = send_packet(&client, &destination, lazy_packet).await {
                    error!(
                        "Sending packet from client \"{}\" failure: {}",
                        &client.name, e
                    );
                }
            }
            None => {
                // connection closed
                break;
            }
        }
    }
    proxy.players.write().unwrap().remove(&client.name);
    client.alive.store(Arc::new(false));
    info!(
        "Client \"{}\", {} connection closed",
        client.name, client_addr
    );
    Ok(())
}

impl SplinterClient {
    pub async fn write_packet_v753(
        &self,
        packet: LazyDeserializedPacket<'_, V753>,
    ) -> anyhow::Result<()> {
        if packet.is_deserialized() {
            self.writer
                .lock()
                .await
                .write_packet_async(
                    packet
                        .into_packet()
                        .map_err(|e| anyhow!(format!("{}", e)))?,
                )
                .await
                .map_err(|e| anyhow!(format!("{}", e)))
        } else {
            self.writer
                .lock()
                .await
                .write_raw_packet_async(packet.into_raw_packet().unwrap())
                .await
                .map_err(|e| anyhow!(e))
        }
    }
    pub async fn send_kick_v753(&self, reason: ClientKickReason) -> anyhow::Result<()> {
        self.write_packet_v753(LazyDeserializedPacket::<V753>::from_packet(
            Packet753::PlayDisconnect(PlayDisconnectSpec {
                reason: Chat::from_text(&reason.text()),
            }),
        ))
        .await
    }
}
