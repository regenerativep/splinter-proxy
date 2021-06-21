use std::{
    collections::HashMap,
    net::{
        SocketAddr,
        TcpStream,
    },
    sync::Arc,
};

use async_compat::Compat;
use async_dup::Arc as AsyncArc;
use craftio_rs::{
    CraftAsyncReader,
    CraftAsyncWriter,
    CraftConnection,
    CraftIo,
    CraftReader,
    CraftWriter,
};
use mcproto_rs::{
    protocol::{
        HasPacketKind,
        RawPacket,
        State,
    },
    types::Chat,
    v1_16_3::{
        HandshakeNextState,
        LoginDisconnectSpec,
        Packet753,
        RawPacket753,
    },
};
use smol::Async;

use crate::{
    proxy::SplinterProxy,
    server::SplinterServer,
};

// The rule here is, you should not have to import anything protocol specific
// outside of their respective module. For example, protocol 753 things from
// mcproto_rs::v1_16_3 stays within v753.rs; nothing should have to import anything
// directly from that specific protocol

pub mod v753;
pub mod v755;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ProtocolVersion {
    V753,
    V754,
    V755,
}

impl ProtocolVersion {
    pub fn from_number(version: i32) -> anyhow::Result<ProtocolVersion> {
        Ok(match version {
            753 => ProtocolVersion::V753,
            754 => ProtocolVersion::V754,
            755 => ProtocolVersion::V755,
            _ => anyhow::bail!("Invalid or unimplemented protocol version \"{}\"", version),
        })
    }
    fn to_number(&self) -> i32 {
        match &self {
            ProtocolVersion::V753 => 753,
            ProtocolVersion::V754 => 754,
            ProtocolVersion::V755 => 755,
        }
    }
}

pub type AsyncCraftConnection =
    CraftConnection<Compat<AsyncArc<Async<TcpStream>>>, Compat<AsyncArc<Async<TcpStream>>>>;
pub type AsyncCraftWriter = CraftWriter<Compat<AsyncArc<Async<TcpStream>>>>;
pub type AsyncCraftReader = CraftReader<Compat<AsyncArc<Async<TcpStream>>>>;

/// Wrapper for a hashmap of tags corresponding to a list of namespaced ids.
#[derive(Clone, Debug)]
pub struct TagList(HashMap<String, Vec<String>>);

/// Contains tags for the tag lists of blocks, items, entities, and fluids.
#[derive(Clone, Debug)]
pub struct Tags {
    pub blocks: TagList,
    pub items: TagList,
    pub fluids: TagList,
    pub entities: TagList,
}

/// Loads a JSON file into a Vec of i32 and String pairs
///
/// Expects the JSON file to be in the format of a list of objects, and each object has a `name`
/// string and an `id` number.
fn load_json_id_name_pairs(data: impl AsRef<str>) -> Vec<(i32, String)> {
    let parsed = match json::parse(data.as_ref()) {
        Ok(parsed) => parsed,
        Err(e) => {
            error!("Failed to parse json: {}", e);
            panic!("File parse error");
        }
    };
    let mut list = vec![];
    for block_data in parsed.members() {
        list.push((
            block_data["id"]
                .as_i32()
                .expect("Failed to convert JSON id to i32"),
            block_data["name"]
                .as_str()
                .expect("Failed to convert JSON name to str")
                .into(),
        ));
    }
    list
}

pub async fn handle_handshake(
    mut conn: AsyncCraftConnection,
    addr: SocketAddr,
    proxy: Arc<SplinterProxy>,
) -> anyhow::Result<()> {
    // yes we're using a specific protocol implementation here, but it should be
    // the same process for all of them, and we choose the protocol
    // we use for the client from here
    let packet = conn.read_packet_async::<RawPacket753>().await?;
    match packet {
        Some(Packet753::Handshake(body)) => {
            match body.next_state {
                HandshakeNextState::Status => match ProtocolVersion::from_number(*body.version) {
                    Ok(ProtocolVersion::V753 | ProtocolVersion::V754) => {
                        v753::handle_client_status(conn, addr, proxy).await?
                    }
                    Ok(ProtocolVersion::V755) => todo!(),
                    Err(e) => {
                        // invalid version, will just fall back to 753
                        v753::handle_client_status(conn, addr, proxy).await?;
                        bail!("Invalid handshake version \"{}\": {}", *body.version, e);
                    }
                },
                HandshakeNextState::Login => match ProtocolVersion::from_number(*body.version) {
                    Ok(ProtocolVersion::V753 | ProtocolVersion::V754) => {
                        v753::handle_client_login(conn, addr, proxy).await?;
                    }
                    Ok(ProtocolVersion::V755) => todo!(),
                    Err(e) => {
                        // invalid version, send login disconnect
                        conn.set_state(State::Login);
                        conn.write_packet_async(Packet753::LoginDisconnect(LoginDisconnectSpec {
                            message: Chat::from_text(
                                &proxy.config.improper_version_disconnect_message,
                            ),
                        }))
                        .await?;
                    }
                },
            }
        }
        Some(other_packet) => bail!(
            "Expected a handshake packet; instead got: {:?}",
            other_packet
        ),
        None => {}
    }
    // match  {
    //     Some(Packet753::Handshake(body)) => match body.next_state {
    //         HandshakeNextState::Status => {
    //             handle_client_status(conn, addr, proxy).await;
    //         }
    //         HandshakeNextState::Login => {
    //             // make sure version is appropriate
    //             let ver = ProtocolVersion::from_number(*body.version)?;
    //             if ver == proxy.protocol {
    //                 handle_client_login(conn, addr, proxy).await
    //             } else {
    //                 bail!(
    //                     "Client has incorrect protocol version to join; client has {}, expected one that satisfies{:?}",
    //                     *body.version,
    //                     proxy.protocol,
    //                 );
    //             }
    //         }
    //     },
    //     Some(packet) => bail!("Got unexpected packet: {:?}", packet),
    //     None => Ok(()),
    // }
    Ok(())
}

pub enum PacketSender {
    Server(Arc<SplinterServer>),
    Proxy,
}

pub trait ConnectionVersion<'a> {
    type Protocol: RawPacket<'a> + HasPacketKind;
}
pub mod version {
    use mcproto_rs::{
        protocol::{
            HasPacketKind,
            RawPacket,
        },
        v1_16_3::RawPacket753,
        v1_17_0::RawPacket755,
    };

    use super::ConnectionVersion;

    pub struct V753;
    impl<'a> ConnectionVersion<'a> for V753 {
        type Protocol = RawPacket753<'a>;
    }
    pub struct V755;
    impl<'a> ConnectionVersion<'a> for V755 {
        type Protocol = RawPacket755<'a>;
    }
}
