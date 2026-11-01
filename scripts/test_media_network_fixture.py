import asyncio
import types
import unittest
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from media_network_fixture import Link


def options(**changes):
    return types.SimpleNamespace(**(dict(start_after=0, burst_every=0, burst=0,
        outage_at=-1, outage_ms=250, loss=0.02, capacity_mbps=30,
        delay_ms=1, jitter_ms=2) | changes))


class NetworkFixtureTests(unittest.IsolatedAsyncioTestCase):
    async def test_actual_loopback_datagrams_cross_both_proxy_directions(self):
        class Echo(asyncio.DatagramProtocol):
            def connection_made(self, transport):self.transport=transport
            def datagram_received(self, data, address):self.transport.sendto(data,address)
        loop=asyncio.get_running_loop()
        echo,_=await loop.create_datagram_endpoint(Echo,local_addr=("127.0.0.1",0))
        port=echo.get_extra_info("sockname")[1]
        with tempfile.TemporaryDirectory(prefix="frames-network-fixture-") as folder:
            process=await asyncio.create_subprocess_exec(sys.executable,str(Path(__file__).with_name("media_network_fixture.py")),
                "--listen","127.0.0.1:0","--target",f"127.0.0.1:{port}","--seconds","1",
                "--loss","0","--delay-ms","0","--jitter-ms","0","--burst","0","--outage-at","-1",
                "--report",str(Path(folder)/"report.json"),stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE,
                creationflags=getattr(subprocess,"CREATE_NO_WINDOW",0))
            ready=json.loads(await asyncio.wait_for(process.stdout.readline(),5))
            received=loop.create_future()
            class Client(asyncio.DatagramProtocol):
                def datagram_received(self,data,_):
                    if not received.done():received.set_result(data)
            client,_=await loop.create_datagram_endpoint(Client,remote_addr=tuple(ready["listen"]))
            try:
                client.sendto(b"generated-fixture")
                self.assertEqual(await asyncio.wait_for(received,1),b"generated-fixture")
                await asyncio.wait_for(process.wait(),3)
                report=json.loads((Path(folder)/"report.json").read_text())
                self.assertEqual(report["upstream"]["forwarded"],1)
                self.assertEqual(report["downstream"]["forwarded"],1)
            finally:
                client.close();echo.close()
                if process.returncode is None:process.kill();await process.wait()
    async def test_seeded_loss_and_reordering_drain_without_retaining_packets(self):
        links = [Link(options(), 6), Link(options(), 6)]
        received = [[], []]
        for index, link in enumerate(links):
            for number in range(500):
                link.submit(number.to_bytes(4, "big"), lambda packet, i=index: received[i].append(int.from_bytes(packet, "big")))
        await asyncio.sleep(0.1)
        self.assertEqual(sorted(received[0]), sorted(received[1]))
        self.assertTrue(450 < len(received[0]) < 500)
        self.assertTrue(all(not link.handles and link.pending_bytes == 0 for link in links))

    async def test_four_packet_burst_cannot_retrigger_forever_on_sparse_handshake_traffic(self):
        link = Link(options(loss=0, burst=4, burst_every=2), 6)
        link.stats["forwarded"] = 32
        link.next_burst = 0
        received = []
        for number in range(8):
            link.submit(bytes([number]), lambda packet: received.append(packet[0]))
        await asyncio.sleep(0.1)
        self.assertEqual(sorted(received), [4, 5, 6, 7])
        self.assertEqual(link.stats["lost"], 4)

    async def test_overload_is_dropped_with_bounded_timers_and_cleanup(self):
        link = Link(options(loss=0, capacity_mbps=1), 6)
        for _ in range(1000):
            link.submit(bytes(1200), lambda _: None)
        self.assertGreater(link.stats["congested"], 0)
        self.assertLessEqual(link.pending_bytes, 256 * 1024)
        self.assertLessEqual(link.stats["max_queue_ms"], 20)
        link.close()
        self.assertEqual(link.pending_bytes, 0)
        self.assertFalse(link.handles)


if __name__ == "__main__":
    unittest.main()
