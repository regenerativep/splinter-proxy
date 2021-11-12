use std::{
    collections::HashMap,
    net::{SocketAddr, TcpListener},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use smol::{
    lock::{Mutex, RwLock},
    Async, Timer,
};

pub mod chat;
pub mod client;
pub mod config;
pub mod logging;
pub mod mapping;
pub mod server;

use client::SplinterClient;
use config::SplinterConfig;
use mapping::SplinterMapping;
use server::SplinterServer;

use crate::{
    protocol::{current::uuid::UUID4, Tags},
    systems::{
        playersave::{
            load_player_data, save_player_data, PlInfo, PlInfoPlayer, PLAYER_DATA_FILENAME,
        },
        zoning::{Zone, Zoner},
    },
};

pub struct SplinterProxy {
    pub alive: AtomicBool,
    pub config: SplinterConfig,
    pub players: RwLock<HashMap<UUID4, Arc<SplinterClient>>>,
    pub servers: RwLock<HashMap<u64, Arc<SplinterServer>>>,
    pub mapping: Mutex<SplinterMapping>,
    pub tags: Mutex<Option<Tags>>,

    pub player_data: Mutex<PlInfo>,
    pub zoner: Zoner,
}

impl SplinterProxy {
    pub fn new(config: SplinterConfig) -> anyhow::Result<Self> {
        let servers = {
            let mut map = HashMap::new();
            for (id, addr_str) in config.simulation_servers.iter() {
                map.insert(
                    *id,
                    Arc::new(SplinterServer {
                        id: *id,
                        address: SocketAddr::from_str(addr_str)?,
                    }),
                );
            }
            RwLock::new(map)
        };
        Ok(Self {
            alive: AtomicBool::new(true),
            config,
            players: RwLock::new(HashMap::new()),
            servers,
            mapping: Mutex::new(SplinterMapping::new()),
            tags: Mutex::new(None),
            zoner: Zoner {
                zones: vec![
                    (
                        0,
                        Zone::Rectangle {
                            x1: -4,
                            z1: -4,
                            x2: 4,
                            z2: 4,
                        },
                    ),
                    (
                        1,
                        Zone::InvertedRectangle {
                            x1: -3,
                            z1: -3,
                            x2: 3,
                            z2: 3,
                        },
                    ),
                ],
            },
            player_data: Mutex::new(
                load_player_data(PLAYER_DATA_FILENAME).unwrap_or(PlInfo::default()),
            ),
        })
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
    pub async fn kick_client(
        &self,
        client_uuid: UUID4,
        reason: ClientKickReason,
    ) -> anyhow::Result<()> {
        let cl_opt = self.players.read().await.get(&client_uuid).map(Arc::clone);
        if let Some(client) = cl_opt {
            client.send_kick(reason).await?;
            client.set_alive(false).await;
            self.players.write().await.remove(&client_uuid);
            let pos = &**client.position.load();
            self.player_data.lock().await.players.insert(
                client.uuid,
                PlInfoPlayer {
                    x: pos.x,
                    y: pos.y,
                    z: pos.z,
                    name: client.name.clone(),
                },
            );
        } else {
            bail!("Failed to find client by UUID \"{}\"", client_uuid);
        }
        Ok(())
    }
    pub async fn shutdown(&self) {
        let uuids = self
            .players
            .read()
            .await
            .iter()
            .map(|(uuid, _)| *uuid)
            .collect::<Vec<UUID4>>();
        if !uuids.is_empty() {
            info!("Disconnecting clients");
            for uuid in uuids {
                if let Err(e) = self.kick_client(uuid, ClientKickReason::Shutdown).await {
                    error!("Error kicking player \"{}\": {}", uuid, e);
                }
            }
        }

        if let Err(e) = save_player_data(&*self.player_data.lock().await, PLAYER_DATA_FILENAME) {
            error!("Error saving player data: {:?}", e);
        }
        info!("Shutting down");
        self.alive.store(false, Ordering::Relaxed);
    }
    pub async fn find_client_by_name(&self, name: impl AsRef<str>) -> Option<Arc<SplinterClient>> {
        let name = name.as_ref();
        return self
            .players
            .read()
            .await
            .iter()
            .find(|(_uuid, cl)| cl.name == name)
            .map(|(_uuid, cl)| Arc::clone(cl));
    }
}

/// A reason for a client to get kicked
#[derive(Clone)]
pub enum ClientKickReason {
    /// Client failed to send a keep alive packet back in time
    TimedOut,
    /// Client was directly kicked
    Kicked(String, Option<String>),
    /// Server shut down
    Shutdown,
}

impl ClientKickReason {
    pub fn text(&self) -> String {
        match self {
            ClientKickReason::TimedOut => "Timed out".into(),
            ClientKickReason::Kicked(by, reason) => format!(
                "Kicked by {}{}",
                by,
                if let Some(reason) = reason {
                    format!(" because \"{}\"", reason)
                } else {
                    "".into()
                }
            ),
            ClientKickReason::Shutdown => "Server shut down".into(),
        }
    }
}

pub async fn run(proxy: Arc<SplinterProxy>) -> anyhow::Result<()> {
    let address = SocketAddr::from_str(proxy.config.proxy_address.as_str())?;
    let listener = Async::<TcpListener>::bind(address)?;
    {
        let proxy = Arc::clone(&proxy);
        smol::spawn(async move {
            info!("Listening for incoming connections on {}", address);
            loop {
                let (stream, addr) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to accept a client: {}", e);
                        continue;
                    }
                };
                if let Err(e) = client::handle(stream, addr, Arc::clone(&proxy)) {
                    error!("Failed to handle connection from {}: {}", addr, e);
                }
            }
        })
        .detach();
    }
    loop {
        if !proxy.is_alive() {
            break;
        }
        Timer::after(Duration::from_secs(1)).await; // sleep so we're not constantly taking up a thread just for this
    }
    Ok(())
}
