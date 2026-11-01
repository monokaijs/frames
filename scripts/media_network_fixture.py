"""Bounded, seeded UDP impairment proxy for real QUIC acceptance fixtures.

It forwards encrypted packets and records counts/timing only. It changes no OS
network settings. Stop with Ctrl+C; all sockets and pending timers are closed.
"""
import argparse
import asyncio
import ctypes
import json
from pathlib import Path
import random
import time


def address(value):
    host, port = value.rsplit(":", 1)
    return host, int(port)


class Link:
    def __init__(self, options, seed):
        self.options, self.random = options, random.Random(seed)
        self.next_wire = 0.0
        self.started = time.monotonic()
        self.started_unix_us = int(time.time() * 1_000_000)
        self.burst_remaining = 0
        self.next_burst = self.started + options.start_after + options.burst_every
        self.handles = set()
        self.pending_bytes = 0
        self.stats = dict(received=0, forwarded=0, lost=0, congested=0, bytes=0, max_queue_ms=0.0, max_dispatch_late_ms=0.0, outage_end_unix_us=self.started_unix_us + int((options.start_after + options.outage_at + options.outage_ms/1000)*1_000_000))

    def submit(self, data, send):
        now = time.monotonic()
        self.stats["received"] += 1
        active = now - self.started >= self.options.start_after
        elapsed = now - self.started - self.options.start_after
        outage = self.options.outage_at >= 0 and self.options.outage_at <= elapsed < self.options.outage_at + self.options.outage_ms / 1000
        if (active and self.options.burst_every > 0 and now >= self.next_burst
                and self.burst_remaining == 0 and self.stats["forwarded"] >= 32):
            self.burst_remaining = self.options.burst
            self.next_burst = float("inf")
        drop = active and (outage or self.burst_remaining > 0 or self.random.random() < self.options.loss)
        if drop:
            if self.burst_remaining:
                self.burst_remaining -= 1
                if not self.burst_remaining:
                    self.next_burst = now + self.options.burst_every
            self.stats["lost"] += 1
            return
        wire = max(now, self.next_wire) + len(data) * 8 / (self.options.capacity_mbps * 1_000_000)
        if wire - now > 0.020 or self.pending_bytes + len(data) > 256 * 1024:
            self.stats["congested"] += 1
            return
        self.next_wire = wire
        delay = (self.options.delay_ms + self.random.random() * self.options.jitter_ms) / 1000 if active else 0
        due = wire + delay
        self.stats["max_queue_ms"] = max(self.stats["max_queue_ms"], (wire - now) * 1000)
        self.pending_bytes += len(data)
        def deliver():
            self.handles.discard(handle)
            self.pending_bytes -= len(data)
            self.stats["forwarded"] += 1
            self.stats["bytes"] += len(data)
            self.stats["max_dispatch_late_ms"] = max(self.stats["max_dispatch_late_ms"], (time.monotonic() - due) * 1000)
            send(data)
        handle = asyncio.get_running_loop().call_later(max(0, due - now), deliver)
        self.handles.add(handle)

    def close(self):
        for handle in self.handles:
            handle.cancel()
        self.handles.clear()
        self.pending_bytes = 0


class Peer(asyncio.DatagramProtocol):
    def __init__(self, front, address, upstream, downstream):
        self.front, self.address, self.upstream, self.downstream = front, address, upstream, downstream

    def connection_made(self, transport):
        self.transport = transport

    def datagram_received(self, data, _):
        self.downstream.submit(data, lambda packet: self.front.sendto(packet, self.address))


async def run(options):
    upstream, downstream = Link(options, options.seed), Link(options, options.seed + 1)
    peers, starting = {}, set()
    class Front(asyncio.DatagramProtocol):
        def connection_made(self, transport):
            self.transport = transport

        def datagram_received(self, data, address):
            if address in peers:
                upstream.submit(data, peers[address].sendto)
            elif address not in starting and len(peers) + len(starting) < 8:
                starting.add(address)
                async def open_peer():
                    try:
                        transport, _ = await asyncio.get_running_loop().create_datagram_endpoint(
                            lambda: Peer(self.transport, address, upstream, downstream), remote_addr=options.target)
                        peers[address] = transport
                        upstream.submit(data, transport.sendto)
                    finally:
                        starting.discard(address)
                asyncio.create_task(open_peer())
    transport, _ = await asyncio.get_running_loop().create_datagram_endpoint(Front, local_addr=options.listen)
    print(json.dumps(dict(event="ready", listen=transport.get_extra_info("sockname"), target=options.target, seed=options.seed)), flush=True)
    try:
        await asyncio.sleep(options.seconds)
    finally:
        upstream.close(); downstream.close(); transport.close()
        for peer in peers.values():
            peer.close()
        report = dict(options=vars(options) | {"report": str(options.report)}, upstream=upstream.stats, downstream=downstream.stats,
                      scope="seeded encrypted UDP impairment; inspect measured dispatch lateness before asserting the network envelope")
        options.report.parent.mkdir(parents=True, exist_ok=True)
        options.report.write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(json.dumps(report), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", type=address, required=True)
    parser.add_argument("--target", type=address, required=True)
    parser.add_argument("--seconds", type=float, default=180)
    parser.add_argument("--seed", type=int, default=6)
    parser.add_argument("--loss", type=float, default=0.02)
    parser.add_argument("--delay-ms", type=float, default=3)
    parser.add_argument("--jitter-ms", type=float, default=5)
    parser.add_argument("--capacity-mbps", type=float, default=30)
    parser.add_argument("--burst", type=int, default=4)
    parser.add_argument("--burst-every", type=float, default=2)
    parser.add_argument("--start-after", type=float, default=30)
    parser.add_argument("--outage-at", type=float, default=60)
    parser.add_argument("--outage-ms", type=float, default=250)
    parser.add_argument("--report", type=Path, default=Path("dist/media-network.json"))
    options = parser.parse_args()
    if not (0 <= options.loss <= 1 and options.capacity_mbps > 0 and options.seconds > 0 and
            min(options.delay_ms, options.jitter_ms, options.start_after, options.outage_ms, options.burst) >= 0):
        parser.error("invalid impairment bounds")
    windows = hasattr(ctypes, "windll")
    if windows:
        ctypes.windll.winmm.timeBeginPeriod(1)
    try:
        asyncio.run(run(options))
    except KeyboardInterrupt:
        pass
    finally:
        if windows:
            ctypes.windll.winmm.timeEndPeriod(1)


if __name__ == "__main__":
    main()
