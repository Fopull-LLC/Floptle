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

> **Raise the open-file limit on the driving machine before a big run.** Every
> simulated player is a socket here, and the default limit of 1024 fails a join
> past about 400 CCU with `Too many open files` — which reads like a relay fault
> and is not one. `ulimit -n 65536` first.

## What it prints

```
--- 100 CCU, 30.0s ---
  sent            12000  (of 12000 the rate asked for)
  echoed          12000
  unreturned      0  (0.00%)
  fanned out      72000 received of 72000 sent to the other members (0 short)
  relay egress    4.91 Mbps  (payload; what it sent on)
  relay ingress   4.91 Mbps  (payload; what was sent to it)
  per CCU         0.0491 Mbps
  implied ceiling 7820 CCU at 80% of 480 Mbps
  round trip p50  8.10 ms
  round trip p95  19.44 ms
```

- **per CCU** — what one player costs in relay egress. A box's ceiling is
  `0.8 × link ÷ this`. Eighty percent because a link at capacity is already
  dropping.
- **unreturned** — packets that never came back. **Anything above zero means you
  are past the limit**, whatever the arithmetic says: those are players losing
  packets.
- **sent (of N)** — how many pings went out against how many the rate asked
  for. A client waits for its echo before sending again, so a relay that is
  stalling shows here as a shortfall before it shows as loss.
- **fanned out** — what the other members of each lobby received of what the
  host sent them.
- **round trip p95** — measured from outside the box, so it is the independent
  check on the relay's own `step_p95_ms`.

**A ceiling is printed only on a clean step.** On a step with anything
unreturned, or with a send rate short of the one asked for, the tool prints
`implied ceiling withheld` and marks `per CCU` as not a cost. That is not
caution for its own sake: the first walk of this curve printed its *highest*
ceiling on the step where the relay was dropping, because stalled senders
lowered the rate, which lowered the per-player figure, which raised the
arithmetic. A ceiling read off a failing step goes up as the relay fails.

Take the relay's own reading for the same window and compare —
`egress_bytes_per_sec` (bytes, not bits: multiply by eight for the link)
against the link, and `rx_drops`, which is the honest ceiling because those
are packets the kernel has already thrown away.

## Walking the curve

```bash
for n in 50 100 200 400; do
  floptle-relay-bench --relay us-east.relay.fopull.com:7788 --ccu "$n" --seconds 60
done
```

Record the relay's `egress_bytes_per_sec`, `rx_drops` and `step_p95_ms` at each step. The
number you are looking for is the CCU at which `rx_drops` **first rises**.

## Traffic shape decides the answer

A lobby multiplies: one packet from one player reaches every other member, so
an eight-player lobby turns each ping into eight packets out of the relay. The
bench's host does what a game host does — echoes the ping to its sender and
sends the same bytes to every other member (`--echo-only` turns that off, for
the older, lighter shape). So the shape matters more than the volume, and a
bench sending big infrequent packets finds a completely different ceiling from
one sending small frequent ones. Games are the second kind, which is the
default here — 128 bytes twenty times a second.

Note where the multiplying happens: **at the host, not inside the relay.** The
wire has no broadcast — the host sends one message per recipient and the relay
forwards each — so the relay's ingress and egress are equal by construction,
and a lobby of eight costs the relay's link sixteen packets per ping, eight in
and eight out.

| flag | default | what it changes |
|---|---|---|
| `--ccu` | 50 | simulated concurrent players |
| `--lobby-size` | 8 | players per lobby, host included — this is the multiplier |
| `--payload` | 128 | bytes per packet (at least 8; the first bytes tag the sender) |
| `--interval` | 50 | milliseconds between a client's packets |
| `--seconds` | 30 | how long to drive it |
| `--link` | 480 | the relay's link in Mbps, for the implied ceiling |
| `--key` | — | a game key. **A managed relay refuses a keyless host**, so a run against one needs this |
| `--unreliable` | off | send on the unreliable channel instead |
| `--echo-only` | off | the host answers only the sender, rather than every member |
| `--verify` | off | drive nothing; check the relay's certificate (below) |

## Checking a relay's certificate

A relay reached by a name under `fopull.com` is verified by every client: it has
to present a chain a public CA issued for that name. `--verify` is how you find
out whether it does, from outside the box, using the same handshake a player's
build runs:

```
floptle-relay-bench --relay us-east.relay.fopull.com:7788 --verify
```

```
us-east.relay.fopull.com:7788 presents 42:07:94:20:F7:12:…:A2:05
VERIFIED — the chain is trusted for us-east.relay.fopull.com by the public roots
```

The first line is the SHA-256 fingerprint of the leaf the relay actually
presented, in the spelling `openssl x509 -noout -fingerprint -sha256` uses, so
you can compare it with the file on the box — after a renewal, that is how you
know the running relay picked it up. The second is the verdict: exit 0 when the
chain verifies for the name, 1 with the reason when it does not. A relay
presenting its self-signed certificate reads `NOT VERIFIED … CaUsedAsEndEntity`.

`openssl s_client` cannot do this check: it speaks TLS over TCP, and a relay
answers QUIC on UDP. Give `--verify` the relay's **name**, not its address — a
certificate is issued for a name, and an address is refused up front.

## Reading a bad run

- **`NOTHING RETURNED`** — no packet made the round trip. The relay is not
  reachable, or refused every host. A managed relay needs `--key`.
- **Every packet unreturned but lobbies opened** — hosts connected and traffic
  did not forward. Check the relay's own log.
- **`implied ceiling withheld`** — the step is past the limit (something
  unreturned, or senders stalled). Read the ceiling off the last clean step
  below it; this one only tells you where the cliff is.
- **An implausibly high implied ceiling on a clean step** — the traffic shape
  is too light for the game you have in mind. Raise `--payload`, lower
  `--interval`, or raise `--lobby-size`, which multiplies hardest.
- **Drops at a small fraction of the link, with the loop fast and the box
  idle** — the relay's inbox, not its link. A UDP socket's receive buffer is
  the kernel default unless asked for, and the default holds a couple of
  hundred datagrams. The relay asks for 8 MiB and prints what it was given at
  startup; if that line says `CLAMPED`, raise `net.core.rmem_max` (and
  `wmem_max`) on the box and restart it.

The host in the loop does nothing but forward. That is deliberate: anything it
spends is noise in a measurement of the relay.
