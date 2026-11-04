// Command loadtest drives real TCP and UDP clients against acserver-rs. Run several
// processes with distinct GUID ranges to avoid the in-process netpoller ceiling:
//
//	./acserver-rs --cfg cfg &
//	./loadtest -addr 127.0.0.1:9600 -clients 128 -guidbase 1000000 -race 5s -finish &
//	./loadtest -addr 127.0.0.1:9600 -clients 127 -guidbase 2000000 -race 5s -finish &
//
// It writes JSON metrics to stdout, a summary to stderr, and exits non-zero on
// join failure or a missing race end when -finish is set.
package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"fmt"
	"net"
	"os"
	"strconv"
	"sync"
	"sync/atomic"
	"time"

	"acserver-rs/test/internal/proto"
	"acserver-rs/test/internal/wire"
)

func main() {
	addr := flag.String("addr", "127.0.0.1:9600", "server host:port (TCP and UDP share it)")
	clients := flag.Int("clients", 64, "number of clients this process drives")
	car := flag.String("car", "ks_mazda_mx5_cup", "car model to request (must be in the entry list)")
	guidBase := flag.Int64("guidbase", 76561190000000000, "first Steam guid; must not overlap other instances")
	name := flag.String("name", "L", "driver-name prefix")
	race := flag.Duration("race", 0, "stream position updates for this long after everyone joins")
	posHz := flag.Int("poshz", 18, "position updates per second per client during -race")
	finish := flag.Bool("finish", false, "after racing, cross the line and wait for the 0x4B RaceOver")
	timeout := flag.Duration("timeout", 60*time.Second, "how long to wait for RaceOver with -finish")
	flag.Parse()

	lt := &loadTest{
		addr: *addr, car: *car, guidBase: *guidBase, name: *name,
		race: *race, posHz: *posHz, finish: *finish, timeout: *timeout,
	}
	os.Exit(lt.run(*clients))
}

type loadTest struct {
	addr     string
	car      string
	guidBase int64
	name     string
	race     time.Duration
	posHz    int
	finish   bool
	timeout  time.Duration
}

type client struct {
	tcp       net.Conn
	br        *bufio.Reader
	udp       net.Conn
	sessionID uint8
	// startsAt is when the session's timed start is, from 0x3E. A race lap
	// before it is dropped (docs/02 "End of a race").
	startsAt time.Time
}

type metrics struct {
	Clients      int     `json:"clients"`
	Joined       int     `json:"joined"`
	JoinMillis   float64 `json:"join_ms"`
	RaceOver     bool    `json:"race_over"`
	RaceOverSeen int     `json:"race_over_seen"`
	RaceOverMs   float64 `json:"race_over_ms"`
	PositionsOut int64   `json:"positions_out"`
}

func (lt *loadTest) run(n int) int {
	m := metrics{Clients: n}

	// --- Join every client (concurrently). ---
	var (
		mu       sync.Mutex
		joined   []*client
		joinErrs int
		wg       sync.WaitGroup
	)
	var raceOverSeen atomic.Int64
	var firstSeenNano atomic.Int64 // unixnano of the first RaceOver any client saw
	var stopDrain = make(chan struct{})

	start := time.Now()
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			c, err := lt.join(i)
			if err != nil {
				mu.Lock()
				joinErrs++
				mu.Unlock()
				return
			}
			mu.Lock()
			joined = append(joined, c)
			mu.Unlock()

			// Drain TCP, and time RaceOver's arrival.
			go func() {
				for {
					pkt, err := wire.ReadTCPPacket(c.br)
					if err != nil {
						return
					}
					if pkt[0] == proto.RaceOver {
						raceOverSeen.Add(1)
						firstSeenNano.CompareAndSwap(0, time.Now().UnixNano())
					}
					select {
					case <-stopDrain:
						return
					default:
					}
				}
			}()
		}(i)
	}
	wg.Wait()
	m.JoinMillis = float64(time.Since(start).Microseconds()) / 1000
	m.Joined = len(joined)

	defer func() {
		close(stopDrain)
		for _, c := range joined {
			c.tcp.Close()
			c.udp.Close()
		}
	}()

	if joinErrs > 0 {
		lt.report(m, fmt.Sprintf("%d/%d clients failed to join", joinErrs, n))
		return 1
	}

	// --- Everyone races at once: stream position updates. ---
	if lt.race > 0 {
		var posOut atomic.Int64
		var rwg sync.WaitGroup
		interval := time.Second / time.Duration(max(lt.posHz, 1))
		for _, c := range joined {
			rwg.Add(1)
			go func(c *client) {
				defer rwg.Done()
				deadline := time.Now().Add(lt.race)
				var seq uint8 = 2
				var norm float32 = 0.1
				for time.Now().Before(deadline) {
					c.udp.Write(buildPosition(seq, norm))
					posOut.Add(1)
					seq++
					if norm += 0.01; norm >= 1 {
						norm = 0
					}
					time.Sleep(interval)
				}
			}(c)
		}
		rwg.Wait()
		m.PositionsOut = posOut.Load()
	}

	// --- Cross the line and wait for the whole field to see the race end. ---
	if lt.finish {
		// Laps before the race starts are dropped, so wait for the green light.
		for _, c := range joined {
			if wait := time.Until(c.startsAt); wait > 0 {
				time.Sleep(wait)
			}
		}
		finishAt := time.Now()
		for _, c := range joined {
			c.tcp.Write(wire.FrameTCP(buildLap(1)))
		}
		// Poll until every joined client has received RaceOver, or we give up.
		deadline := time.After(lt.timeout)
		poll := time.NewTicker(50 * time.Millisecond)
		for int(raceOverSeen.Load()) < len(joined) {
			select {
			case <-deadline:
				poll.Stop()
				goto finishDone
			case <-poll.C:
			}
		}
		poll.Stop()
	finishDone:
		m.RaceOverSeen = int(raceOverSeen.Load())
		m.RaceOver = m.RaceOverSeen > 0
		if t0 := firstSeenNano.Load(); t0 != 0 {
			m.RaceOverMs = float64(time.Unix(0, t0).Sub(finishAt).Microseconds()) / 1000
		}
		if m.RaceOverSeen < len(joined) {
			lt.report(m, fmt.Sprintf("only %d/%d clients saw RaceOver within %s",
				m.RaceOverSeen, m.Joined, lt.timeout))
			return 1
		}
	}

	lt.report(m, "")
	return 0
}

// join runs a full connect: 0x3D handshake, UDP 0x4E association, 0x44
// checksum, and the first 0x46 position update that puts the car in the world.
func (lt *loadTest) join(i int) (*client, error) {
	tcp, err := net.Dial("tcp", lt.addr)
	if err != nil {
		return nil, fmt.Errorf("dial tcp: %w", err)
	}
	br := bufio.NewReader(tcp)

	guid := strconv.FormatInt(lt.guidBase+int64(i), 10)
	hs := wire.NewWriterID(proto.RequestNewConnection)
	hs.UInt16(proto.ClientProtocolVersion)
	hs.String8(guid)
	hs.WString8(fmt.Sprintf("%s-%d", lt.name, i))
	hs.String8("")
	hs.String8("GBR")
	hs.String8(lt.car)
	hs.String8("")
	tcp.Write(wire.FrameTCP(hs.Bytes()))

	tcp.SetReadDeadline(time.Now().Add(10 * time.Second))
	resp, err := wire.ReadTCPPacket(br)
	if err != nil {
		tcp.Close()
		return nil, fmt.Errorf("read 0x3E: %w", err)
	}
	if resp[0] != proto.NewCarConnection {
		tcp.Close()
		return nil, fmt.Errorf("expected 0x3E, got 0x%02X", resp[0])
	}
	sessionID, elapsedMs := parseHandshake(resp[1:])
	startsAt := time.Now().Add(-time.Duration(elapsedMs) * time.Millisecond)
	tcp.SetReadDeadline(time.Time{})

	udp, err := net.Dial("udp", lt.addr)
	if err != nil {
		tcp.Close()
		return nil, fmt.Errorf("dial udp: %w", err)
	}
	assoc := wire.NewWriterID(proto.CarConnect)
	assoc.UInt8(sessionID)
	udp.Write(assoc.Bytes())
	udp.SetReadDeadline(time.Now().Add(5 * time.Second))
	if _, err := udp.Read(make([]byte, 64)); err != nil {
		tcp.Close()
		udp.Close()
		return nil, fmt.Errorf("udp assoc reply: %w", err)
	}
	udp.SetReadDeadline(time.Time{})

	cs := wire.NewWriterID(proto.ChecksumPacket)
	cs.Raw(make([]byte, 16))
	tcp.Write(wire.FrameTCP(cs.Bytes()))

	udp.Write(buildPosition(1, 0.1))

	return &client{tcp: tcp, br: br, udp: udp, sessionID: sessionID, startsAt: startsAt}, nil
}

// parseHandshake reads the assigned session id and the elapsed session time
// (negative before a race starts) from 0x3E (docs/02 0x3E).
func parseHandshake(payload []byte) (uint8, int64) {
	r := wire.NewReader(payload)
	r.WString8() // server name
	r.UInt16()   // udp port
	r.UInt8()    // refresh rate
	r.String8()  // track
	r.String8()  // track config
	r.String8()  // car model
	r.String8()  // skin
	r.Float32()  // sun angle
	r.Int16()    // allowed tyres out
	r.Bool()     // tyre blankets
	r.UInt8()    // traction control
	r.UInt8()    // abs
	r.Bool()     // stability
	r.Bool()     // autoclutch
	r.UInt8()    // start rule
	r.Float32()  // mechanical damage rate
	r.Float32()  // fuel rate
	r.Float32()  // tyre wear rate
	r.Bool()     // force virtual mirror
	r.UInt8()    // max contacts per km
	r.Int32()    // race over time
	r.Int32()    // result screen time
	r.Bool()     // race extra lap
	r.Bool()     // race gas penalty disabled
	r.Int16()    // pit window start
	r.Int16()    // pit window end
	r.Int16()    // reversed grid race positions
	sessionID := r.UInt8()
	for n := r.UInt8(); n > 0; n-- {
		r.UInt8()  // type
		r.UInt16() // laps
		r.UInt16() // minutes
	}
	r.String8() // current session name
	r.UInt8()   // index
	r.UInt8()   // type
	r.UInt16()  // minutes
	r.UInt16()  // laps
	r.Float32() // grip
	r.UInt8()   // spawn position
	return sessionID, r.Int64()
}

// buildPosition encodes an inbound 0x46 update (61-byte payload, 62-byte datagram).
func buildPosition(seq uint8, norm float32) []byte {
	p := wire.NewWriterID(proto.PositionUpdate)
	p.UInt8(seq)
	p.UInt32(1000)
	p.Vec3(wire.Vec3{})
	p.Vec3(wire.Vec3{})
	p.Vec3(wire.Vec3{})
	p.Raw(make([]byte, 4)) // tyre angular speeds
	p.UInt8(127)           // steer
	p.UInt8(127)           // wheel
	p.UInt16(3000)         // rpm
	p.UInt8(2)             // gear
	p.UInt32(0)            // status
	p.Int16(0)             // performance delta
	p.UInt8(0)             // gas
	p.Float32(norm)        // normalized position
	return p.Bytes()
}

// buildLap encodes an inbound 0x49 lap-completed for the given lap number.
func buildLap(numLap uint8) []byte {
	lap := wire.NewWriterID(proto.LapCompleted)
	lap.UInt32(50000) // timestamp
	lap.UInt32(95000) // lap time
	lap.UInt8(0)      // split count
	lap.UInt8(0)      // cuts
	lap.UInt8(numLap)
	return lap.Bytes()
}

func (lt *loadTest) report(m metrics, failure string) {
	b, _ := json.Marshal(m)
	fmt.Println(string(b))
	if failure != "" {
		fmt.Fprintf(os.Stderr, "loadtest FAIL: %s | joined %d/%d in %.0fms\n",
			failure, m.Joined, m.Clients, m.JoinMillis)
		return
	}
	msg := fmt.Sprintf("loadtest OK: joined %d/%d in %.0fms", m.Joined, m.Clients, m.JoinMillis)
	if lt.race > 0 {
		msg += fmt.Sprintf(", %d positions sent", m.PositionsOut)
	}
	if lt.finish {
		msg += fmt.Sprintf(", RaceOver in %.0fms (%d clients)", m.RaceOverMs, m.RaceOverSeen)
	}
	fmt.Fprintln(os.Stderr, msg)
}
