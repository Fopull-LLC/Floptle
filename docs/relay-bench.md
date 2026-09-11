# Measuring a relay

`floptle-relay-bench` drives synthetic players through a relay and reports what
one player costs. It exists because two decisions depend on a number nobody had:
whether the free tier can be raised, and whether a region's relay is on the right
machine.

Download it from a release beside `floptle-relay` — `floptle-relay-bench-<version>-linux-x86_64`
and `-linux-aarch64`. Run it from a machine in the **same region** as the relay:
driving one from a laptop measures your own upstream, not the relay's link.

```
floptle-relay-bench --relay us-east.relay.fopull.com:7788 --ccu 100
```

> **Run the relay under test with `--no-address-limits`.** A relay refuses more
> than ten lobby opens and thirty joins a minute from one address, and a bench
> run from one machine is exactly that shape. The flag lifts the two
> per-address rates and nothing else; a relay serving players never needs it.

## What it prints

```
--- 100 CCU, 30.0s ---
  sent            12000
  echoed          11998
  unreturned      2  (0.02%)
  payload rate    4.91 Mbps across the relay
  per CCU         0.0491 Mbps
  implied ceiling 7820 CCU at 80% of 480 Mbps
  round trip p50  8.10 ms
  round trip p95  19.44 ms
```

- **per CCU** — what one player costs. A box's ceiling is `0.8 × link ÷ this`.
  Eighty percent because a link at capacity is already dropping.
- **unreturned** — packets that never came back. **Anything above zero means you
  are past the limit**, whatever the arithmetic says: those are players losing
  packets.
- **round trip p95** — measured from outside the box, so it is the independent
  check on the relay's own `step_p95_ms`.

Take the relay's own reading for the same window and compare — `egress_bps`
against the link, and `rx_drops`, which is the honest ceiling because those are
packets the kernel has already thrown away.

## Walking the curve

```bash
for n in 50 100 200 400; do
  floptle-relay-bench --relay us-east.relay.fopull.com:7788 --ccu "$n" --seconds 60
done
```

Record the relay's `egress_bps`, `rx_drops` and `step_p95_ms` at each step. The
number you are looking for is the CCU at which `rx_drops` **first rises**.

## Traffic shape decides the answer

A relay multiplies: one datagram into an eight-player lobby leaves seven times.
So the shape matters more than the volume, and a bench sending big infrequent
packets finds a completely different ceiling from one sending small frequent
ones. Games are the second kind, which is the default here — 128 bytes twenty
times a second.

| flag | default | what it changes |
|---|---|---|
| `--ccu` | 50 | simulated concurrent players |
| `--lobby-size` | 8 | players per lobby, host included — this is the multiplier |
| `--payload` | 128 | bytes per packet |
| `--interval` | 50 | milliseconds between a client's packets |
| `--seconds` | 30 | how long to drive it |
| `--link` | 480 | the relay's link in Mbps, for the implied ceiling |
| `--key` | — | a game key. **A managed relay refuses a keyless host**, so a run against one needs this |
| `--unreliable` | off | send on the unreliable channel instead |

## Reading a bad run

- **`NOTHING RETURNED`** — no packet made the round trip. The relay is not
  reachable, or refused every host. A managed relay needs `--key`.
- **Every packet unreturned but lobbies opened** — hosts connected and traffic
  did not forward. Check the relay's own log.
- **An implausibly high implied ceiling** — the traffic shape is too light for
  the game you have in mind. Raise `--payload`, lower `--interval`, or raise
  `--lobby-size`, which multiplies hardest.

The host in the loop does nothing but echo. That is deliberate: anything it
spends is noise in a measurement of the relay.
