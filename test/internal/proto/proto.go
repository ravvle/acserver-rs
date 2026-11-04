// Package proto holds the packet ids and enums (docs/01 §1.4).
package proto

// ClientProtocolVersion is the AC 1.x network protocol version.
const ClientProtocolVersion uint16 = 202

// Game protocol packet IDs (ACServerProtocol enum).
const (
	P2PUpdate            byte = 0x0D
	MandatoryPitUpdate   byte = 0x0E
	Blacklisted          byte = 0x3B
	WrongPassword        byte = 0x3C
	RequestNewConnection byte = 0x3D
	NewCarConnection     byte = 0x3E
	CarListRequest       byte = 0x3F
	CarList              byte = 0x40
	ServerRunning        byte = 0x41
	UnsupportedProtocol  byte = 0x42
	CleanExitDrive       byte = 0x43
	ChecksumPacket       byte = 0x44
	NoSlotsAvailable     byte = 0x45
	PositionUpdate       byte = 0x46
	Chat                 byte = 0x47
	MegaPacket           byte = 0x48
	LapCompleted         byte = 0x49
	CurrentSessionUpdate byte = 0x4A
	RaceOver             byte = 0x4B
	Pulse                byte = 0x4C
	CarDisconnected      byte = 0x4D
	CarConnect           byte = 0x4E // UDP association
	SessionRequest       byte = 0x4F
	TyreCompoundChange   byte = 0x50
	WelcomeMessage       byte = 0x51
	CarSetup             byte = 0x52
	DrsZonesUpdate       byte = 0x53
	SunAngleUpdate       byte = 0x54
	DamageUpdate         byte = 0x56
	RaceStart            byte = 0x57
	SectorSplit          byte = 0x58
	CarConnected         byte = 0x5A
	DriverInfoUpdate     byte = 0x5B
	VoteNextSession      byte = 0x64
	VoteRestartSession   byte = 0x65
	VoteKickUser         byte = 0x66
	VoteQuorumNotReached byte = 0x67
	KickCar              byte = 0x68
	SessionClosed        byte = 0x6E
	AuthFailed           byte = 0x6F
	BoPUpdate            byte = 0x70
	WeatherUpdate        byte = 0x78
	ClientEvent          byte = 0x82
	Extended             byte = 0xAB // CSP
	LobbyCheck           byte = 0xC8
	PingPong             byte = 0xF8
	PingUpdate           byte = 0xF9
)

// KickReason values used in the 0x68 KickCar packet.
const (
	KickVoteKicked      byte = 0
	KickVoteBanned      byte = 1
	KickVoteBlacklisted byte = 2
	KickChecksumFailed  byte = 3
	KickKicked          byte = 4
)

// Extended (0xAB) sub-types, which depend on the transport (docs/04 §4.1).
const (
	ExtTCPSpectateCar   byte = 0x00 // C→S
	ExtTCPCarVisibility byte = 0x02 // S→C
	ExtTCPClientMessage byte = 0x03 // both
	ExtTCPKickMessage   byte = 0x05 // S→C
	ExtUDPWeatherUpdate byte = 0x01 // S→C, WeatherFX
	ExtUDPCustomUpdate  byte = 0x03 // S→C, position batch
	ExtUDPClientMessage byte = 0x05 // both
)

// CSP client message types (docs/04 §4.3).
const (
	CSPHandshakeIn     uint16 = 0
	CSPHandshakeOut    uint16 = 1
	CSPCarColorChange  uint16 = 15000
	CSPAdminPenalty    uint16 = 50000
	CSPLuaMessage      uint16 = 60000
	CSPLuaTargeted     uint16 = 60001
	CSPLuaRanged       uint16 = 60002
	CSPLuaRangedTarget uint16 = 60003
)

// ClientEvent sub-types (0x82).
const (
	EventCollisionWithCar byte = 0x0A
	EventCollisionWithEnv byte = 0x0B
	EventJumpStart        byte = 0x0C
)

// SessionType values.
const (
	SessionBooking  byte = 0
	SessionPractice byte = 1
	SessionQualify  byte = 2
	SessionRace     byte = 3
)

// ServerSessionID is the broadcast/"server" session id.
const ServerSessionID byte = 0xFF

// MaxCarSlots is the number of assignable car session ids. Cars use 0..254;
// 0xFF is reserved for server/broadcast packets.
const MaxCarSlots = int(ServerSessionID)

// CarStatusFlags bit masks (docs/03-udp-protocol.md §3.3).
const (
	StatusBrakeLights  uint32 = 0x0000_0010
	StatusLights       uint32 = 0x0000_0020
	StatusHorn         uint32 = 0x0000_0040
	StatusIndicateLeft uint32 = 0x0000_0800
	StatusIndicateRght uint32 = 0x0000_1000
	StatusHazards      uint32 = 0x0000_2000
	StatusHighBeamOff  uint32 = 0x0000_4000
)
