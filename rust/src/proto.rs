//! Packet ids and enums (docs/01 §1.4): the single source for every `0x..`
//! value on the wire.

/// The AC 1.x network protocol version.
pub const CLIENT_PROTOCOL_VERSION: u16 = 202;

// Game protocol packet ids (ACServerProtocol).
pub const P2P_UPDATE: u8 = 0x0D;
pub const MANDATORY_PIT_UPDATE: u8 = 0x0E;
pub const BLACKLISTED: u8 = 0x3B;
pub const WRONG_PASSWORD: u8 = 0x3C;
pub const REQUEST_NEW_CONNECTION: u8 = 0x3D;
pub const NEW_CAR_CONNECTION: u8 = 0x3E;
pub const CAR_LIST_REQUEST: u8 = 0x3F;
pub const CAR_LIST: u8 = 0x40;
pub const SERVER_RUNNING: u8 = 0x41;
pub const UNSUPPORTED_PROTOCOL: u8 = 0x42;
pub const CLEAN_EXIT_DRIVE: u8 = 0x43;
pub const CHECKSUM_PACKET: u8 = 0x44;
pub const NO_SLOTS_AVAILABLE: u8 = 0x45;
pub const POSITION_UPDATE: u8 = 0x46;
pub const CHAT: u8 = 0x47;
pub const MEGA_PACKET: u8 = 0x48;
pub const LAP_COMPLETED: u8 = 0x49;
pub const CURRENT_SESSION_UPDATE: u8 = 0x4A;
pub const RACE_OVER: u8 = 0x4B;
pub const PULSE: u8 = 0x4C;
pub const CAR_DISCONNECTED: u8 = 0x4D;
/// UDP association.
pub const CAR_CONNECT: u8 = 0x4E;
pub const SESSION_REQUEST: u8 = 0x4F;
pub const TYRE_COMPOUND_CHANGE: u8 = 0x50;
pub const WELCOME_MESSAGE: u8 = 0x51;
pub const CAR_SETUP: u8 = 0x52;
pub const DRS_ZONES_UPDATE: u8 = 0x53;
pub const SUN_ANGLE_UPDATE: u8 = 0x54;
pub const DAMAGE_UPDATE: u8 = 0x56;
pub const RACE_START: u8 = 0x57;
pub const SECTOR_SPLIT: u8 = 0x58;
pub const CAR_CONNECTED: u8 = 0x5A;
pub const DRIVER_INFO_UPDATE: u8 = 0x5B;
pub const VOTE_NEXT_SESSION: u8 = 0x64;
pub const VOTE_RESTART_SESSION: u8 = 0x65;
pub const VOTE_KICK_USER: u8 = 0x66;
pub const VOTE_QUORUM_NOT_REACHED: u8 = 0x67;
pub const KICK_CAR: u8 = 0x68;
pub const SESSION_CLOSED: u8 = 0x6E;
pub const AUTH_FAILED: u8 = 0x6F;
pub const BOP_UPDATE: u8 = 0x70;
pub const WEATHER_UPDATE: u8 = 0x78;
pub const CLIENT_EVENT: u8 = 0x82;
/// CSP extension packets.
pub const EXTENDED: u8 = 0xAB;
pub const LOBBY_CHECK: u8 = 0xC8;
pub const PING_PONG: u8 = 0xF8;
pub const PING_UPDATE: u8 = 0xF9;

// KickReason values in the 0x68 KickCar packet.
pub const KICK_VOTE_KICKED: u8 = 0;
pub const KICK_VOTE_BANNED: u8 = 1;
pub const KICK_VOTE_BLACKLISTED: u8 = 2;
pub const KICK_CHECKSUM_FAILED: u8 = 3;
pub const KICK_KICKED: u8 = 4;

// Extended (0xAB) sub-types, which depend on the transport (docs/04 §4.1).
pub const EXT_TCP_SPECTATE_CAR: u8 = 0x00; // C→S
pub const EXT_TCP_CAR_VISIBILITY: u8 = 0x02; // S→C
pub const EXT_TCP_CLIENT_MESSAGE: u8 = 0x03; // both
pub const EXT_TCP_KICK_MESSAGE: u8 = 0x05; // S→C

pub const EXT_UDP_WEATHER_UPDATE: u8 = 0x01; // S→C, WeatherFX
pub const EXT_UDP_CUSTOM_UPDATE: u8 = 0x03; // S→C, position batch
pub const EXT_UDP_CLIENT_MESSAGE: u8 = 0x05; // both

// CSP client message types (docs/04 §4.3).
pub const CSP_HANDSHAKE_IN: u16 = 0;
pub const CSP_HANDSHAKE_OUT: u16 = 1;
pub const CSP_CAR_COLOR_CHANGE: u16 = 15000;
pub const CSP_ADMIN_PENALTY: u16 = 50000;
pub const CSP_LUA_MESSAGE: u16 = 60000;
pub const CSP_LUA_TARGETED: u16 = 60001;
pub const CSP_LUA_RANGED: u16 = 60002;
pub const CSP_LUA_RANGED_TARGET: u16 = 60003;

// ClientEvent (0x82) sub-types.
pub const EVENT_COLLISION_WITH_CAR: u8 = 0x0A;
pub const EVENT_COLLISION_WITH_ENV: u8 = 0x0B;
pub const EVENT_JUMP_START: u8 = 0x0C;

/// The broadcast / "server" session id.
pub const SERVER_SESSION_ID: u8 = 0xFF;

/// The number of assignable car session ids. Cars use 0..254; 0xFF is
/// reserved for server and broadcast packets.
pub const MAX_CAR_SLOTS: usize = SERVER_SESSION_ID as usize;

// CarStatusFlags bit masks (docs/03 §3.3).
pub const STATUS_BRAKE_LIGHTS: u32 = 0x0000_0010;
pub const STATUS_LIGHTS: u32 = 0x0000_0020;
pub const STATUS_HORN: u32 = 0x0000_0040;
pub const STATUS_INDICATE_LEFT: u32 = 0x0000_0800;
pub const STATUS_INDICATE_RIGHT: u32 = 0x0000_1000;
pub const STATUS_HAZARDS: u32 = 0x0000_2000;
pub const STATUS_HIGH_BEAM_OFF: u32 = 0x0000_4000;

/// A session's type, sent as one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionType {
    Booking,
    Practice,
    Qualify,
    Race,
    /// Any other byte. Only ACSP SetSessionInfo can store one, and it is kept
    /// and sent as given.
    Other(u8),
}

impl SessionType {
    /// The wire value.
    pub fn as_u8(self) -> u8 {
        match self {
            SessionType::Booking => 0,
            SessionType::Practice => 1,
            SessionType::Qualify => 2,
            SessionType::Race => 3,
            SessionType::Other(v) => v,
        }
    }

    /// The type for a wire value.
    pub fn from_u8(v: u8) -> SessionType {
        match v {
            0 => SessionType::Booking,
            1 => SessionType::Practice,
            2 => SessionType::Qualify,
            3 => SessionType::Race,
            other => SessionType::Other(other),
        }
    }

    /// The vanilla default session name for this type.
    pub fn default_name(self) -> &'static str {
        match self {
            SessionType::Booking => "Booking",
            SessionType::Practice => "Practice",
            SessionType::Qualify => "Qualify",
            SessionType::Race => "Race",
            SessionType::Other(_) => "Session",
        }
    }

    /// The results file's label (`BOOK`, `PRACTICE`, ...).
    pub fn results_name(self) -> &'static str {
        match self {
            SessionType::Booking => "BOOK",
            SessionType::Practice => "PRACTICE",
            SessionType::Qualify => "QUALIFY",
            SessionType::Race => "RACE",
            SessionType::Other(_) => "SESSION",
        }
    }
}
