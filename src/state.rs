use std::{
    self,
    collections::HashMap,
    fs,
    iter::FromIterator,
    net::{
        SocketAddr,
        TcpStream,
    },
    path::Path,
    sync::{
        Arc,
        Mutex,
        RwLock,
    },
};

use bimap::hash::BiHashMap;
use craftio_rs::{
    CraftSyncWriter,
    CraftWriter,
    WriteError,
};
use json;
use mcproto_rs::{
    protocol::{
        HasPacketKind,
        PacketErr,
    },
    types::{
        Chat,
        CountedArray,
        VarInt,
    },
    uuid::UUID4,
    v1_16_3::{
        ClientChatMode,
        PlayClientSettingsSpec,
        PlayTagsSpec,
        TagSpec,
    },
};

use crate::{
    commands::SplinterCommand,
    config::SplinterProxyConfiguration,
    connection::write_packet_client,
    mapping::{
        ClientPacketMap,
        IdGenerator,
        PacketMap,
        ServerPacketMap,
    },
    proto::{
        PacketLatest,
        PacketLatestKind,
    },
    zoning::Zoner,
};

/// Path to Minecraft block data
pub const BLOCK_MAP_PATH: &str = "./minecraft-data/data/pc/1.16.2/blocks.json";
/// Path to Minecraft item data
pub const ITEM_MAP_PATH: &str = "./minecraft-data/data/pc/1.16.2/items.json";
/// Path to Minecraft entity data
pub const ENTITY_MAP_PATH: &str = "./minecraft-data/data/pc/1.16.2/entities.json";
/// Path to Minecraft fluid data
pub const FLUID_MAP_PATH: &str = "./fluids.json";

lazy_static! {
    pub static ref BLOCK_TYPE_MAP: BiHashMap<i32, String> =
        BiHashMap::<i32, String>::from_iter(load_json_id_name_pairs(BLOCK_MAP_PATH));
    pub static ref ITEM_TYPE_MAP: BiHashMap<i32, String> =
        BiHashMap::<i32, String>::from_iter(load_json_id_name_pairs(ITEM_MAP_PATH));
    pub static ref ENTITY_TYPE_MAP: BiHashMap<i32, String> =
        BiHashMap::<i32, String>::from_iter(load_json_id_name_pairs(ENTITY_MAP_PATH));
    pub static ref FLUID_TYPE_MAP: BiHashMap<i32, String> =
        BiHashMap::<i32, String>::from_iter(load_json_id_name_pairs(FLUID_MAP_PATH));
}

/// Loads a JSON file into a Vec of i32 and String pairs
///
/// Expects the JSON file to be in the format of a list of objects, and each object has a `name`
/// string and an `id` number.
fn load_json_id_name_pairs(filepath: &str) -> Vec<(i32, String)> {
    let data = match fs::read_to_string(filepath) {
        Ok(data) => data,
        Err(e) => {
            error!("Failed to load from file \"{}\": {}", filepath, e);
            panic!("File load error");
        }
    };
    let parsed = match json::parse(data.as_str()) {
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

/// Data associated with an entity
pub struct EntityData {
    pub id: i32,
    pub entity_type: i32,
}

/// Global state for the splinter proxy
pub struct SplinterState {
    /// Whether the proxy is alive
    pub alive: RwLock<bool>,
    /// Zoner for the world
    pub zoner: RwLock<Zoner>,
    /// Configuration
    pub config: RwLock<SplinterProxyConfiguration>,
    /// List of client states
    pub players: RwLock<HashMap<u64, Arc<SplinterClient>>>,
    pub player_id_gen: Mutex<IdGenerator>,
    /// List of servers
    pub servers: RwLock<HashMap<u64, SplinterServer>>,
    pub server_id_gen: Mutex<IdGenerator>,
    /// Client-proxy packet map
    pub client_packet_map: ClientPacketMap,
    /// Proxy-server packet map
    pub server_packet_map: ServerPacketMap,
    /// Proxy-wide tags for the clients
    pub tags: RwLock<Option<Tags>>,
    /// Table to map between proxy entity ids and server entity ids
    pub eid_table: RwLock<BiHashMap<i32, (u64, i32)>>,
    /// Manages available EIDs
    pub eid_gen: Mutex<IdGenerator>,
    /// Extra necessary data associated with entity ids
    pub eid_data: RwLock<HashMap<i32, EntityData>>,
    /// Table to map between proxy uuids and server uids
    pub uuid_table: RwLock<BiHashMap<UUID4, (u64, UUID4)>>,
    /// Hashmap of commands
    pub commands: HashMap<String, SplinterCommand>,
}

/// Server state
pub struct SplinterServer {
    /// Internal unique ID of the server
    pub id: u64,
    /// Server address
    pub addr: SocketAddr,
}

/// Client state
pub struct SplinterClient {
    /// Internal unique ID of the client
    pub id: u64,
    /// Username of the client
    pub name: String,
    /// Server id that the player is in
    pub active_server: RwLock<u64>,
    /// List of connections to servers
    pub servers: RwLock<HashMap<u64, Arc<SplinterServerConnection>>>,
    /// Writer to the client
    pub writer: Mutex<CraftWriter<TcpStream>>,
    /// Proxy's UUID of the client
    pub uuid: UUID4,
    /// Whether the client connection is alive
    pub alive: RwLock<bool>,
    /// Client-side settings
    pub settings: PlayClientSettingsSpec,
    /// List of millisecond keep alive IDs that are being waited for from a client, sorted by
    /// oldest to newest
    pub keep_alive_waiting: RwLock<Vec<i64>>,
}

/// Server connection state specific to client-proxy-server
pub struct SplinterServerConnection {
    /// Internal unique ID of the server
    pub id: u64,
    /// Address of the server
    pub addr: SocketAddr,
    /// Writer to the server
    pub writer: Mutex<CraftWriter<TcpStream>>,
    /// Server's UUID for the client
    pub client_uuid: UUID4,
}

// impl SplinterServerConnection {
// pub fn server<'a>(&self, state: &'a SplinterState) -> &'a SplinterServer {
// state.servers.read().unwrap().get(&self.id).unwrap()
// }
// }

impl SplinterState {
    /// Creates a new splinter state given the proxy configuration
    pub fn new(config: SplinterProxyConfiguration, zoner: Zoner) -> SplinterState {
        SplinterState {
            alive: RwLock::new(true),
            zoner: RwLock::new(zoner),
            config: RwLock::new(config),
            players: RwLock::new(HashMap::new()),
            player_id_gen: Mutex::new(IdGenerator::new()),
            servers: RwLock::new(HashMap::new()),
            server_id_gen: Mutex::new(IdGenerator::new()),
            client_packet_map: PacketMap(HashMap::new()),
            server_packet_map: PacketMap(HashMap::new()),
            tags: RwLock::new(None),
            eid_table: RwLock::new(BiHashMap::new()),
            eid_gen: Mutex::new(IdGenerator::new()),
            eid_data: RwLock::new(HashMap::new()),
            uuid_table: RwLock::new(BiHashMap::new()),
            commands: HashMap::new(),
        }
    }
    pub fn find_client_by_name(&self, name: &str) -> Option<Arc<SplinterClient>> {
        self.players
            .read()
            .unwrap()
            .iter()
            .find(|(_, client)| client.name == name)
            .map(|(_, client_arc)| Arc::clone(client_arc))
    }
}

impl SplinterClient {
    /// Returns an Arc to the active server connection of a client
    pub fn server(&self) -> Arc<SplinterServerConnection> {
        Arc::clone(
            &*self
                .servers
                .read()
                .unwrap()
                .get(&*self.active_server.read().unwrap())
                .unwrap(),
        )
    }
}

pub fn proto_tags_to_tags(
    proto_tags: &CountedArray<TagSpec, VarInt>,
    map: &BiHashMap<i32, String>,
) -> TagList {
    let mut list = HashMap::new();
    for tag in proto_tags.iter() {
        list.insert(
            tag.name.clone(),
            tag.entries
                .iter()
                .map(|val| map.get_by_left(&**val).unwrap().clone())
                .collect::<Vec<String>>(),
        );
    }
    TagList(list)
}

pub fn tags_to_proto_tags(
    tags: &TagList,
    map: &BiHashMap<i32, String>,
) -> CountedArray<TagSpec, VarInt> {
    let mut list = vec![];
    for (name, ids) in tags.0.iter() {
        list.push(TagSpec {
            name: name.clone(),
            entries: ids
                .iter()
                .map(|id| VarInt::from(*map.get_by_right(id).unwrap()))
                .collect::<Vec<VarInt>>()
                .into(),
        });
    }
    list.into()
}

impl From<&PlayTagsSpec> for Tags {
    fn from(proto_tags: &PlayTagsSpec) -> Tags {
        Tags {
            blocks: proto_tags_to_tags(&proto_tags.block_tags, &*BLOCK_TYPE_MAP),
            items: proto_tags_to_tags(&proto_tags.item_tags, &*ITEM_TYPE_MAP),
            fluids: proto_tags_to_tags(&proto_tags.fluid_tags, &*FLUID_TYPE_MAP),
            entities: proto_tags_to_tags(&proto_tags.entity_tags, &*ENTITY_TYPE_MAP),
        }
    }
}

impl From<&Tags> for PlayTagsSpec {
    fn from(tags: &Tags) -> PlayTagsSpec {
        PlayTagsSpec {
            block_tags: tags_to_proto_tags(&tags.blocks, &*BLOCK_TYPE_MAP),
            item_tags: tags_to_proto_tags(&tags.items, &*ITEM_TYPE_MAP),
            fluid_tags: tags_to_proto_tags(&tags.fluids, &*FLUID_TYPE_MAP),
            entity_tags: tags_to_proto_tags(&tags.entities, &*ENTITY_TYPE_MAP),
        }
    }
}

pub fn init(_state: &mut SplinterState) {
    &*BLOCK_TYPE_MAP;
    &*ITEM_TYPE_MAP;
    &*FLUID_TYPE_MAP;
    &*ENTITY_TYPE_MAP;
    debug!("Loaded block, item, fluid, and entity data");
}
