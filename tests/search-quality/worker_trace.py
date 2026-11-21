#!/usr/bin/env python3
"""Record the two ANN rankings and router decision in the isolated evaluation."""

import asyncio
import json
import os
import struct
import sys
from pathlib import Path


async def main(trace):
    daemon_socket = Path(sys.argv[sys.argv.index("--socket") + 1])
    local = daemon_socket.with_suffix(".trace")
    connected = asyncio.get_running_loop().create_future()
    server = await asyncio.start_unix_server(
        lambda r, w: connected.set_result((r, w)), local
    )
    async with server:
        proc = await asyncio.create_subprocess_exec(
            os.environ["QUALITY_WORKER"], "--socket", local
        )
        daemon = await asyncio.open_unix_connection(daemon_socket)
        worker = await asyncio.wait_for(connected, 20)
        pending = {}

        async def relay(reader, writer, requests):
            while True:
                header = await reader.readexactly(6)
                length, _ = struct.unpack(">IH", header)
                body = await reader.readexactly(length - 2)
                msg = json.loads(body)
                key = msg.get("req_id")
                if requests and msg["kind"] in (
                    "search_text",
                    "search_image",
                    "classify_query",
                ):
                    pending[key] = (msg["query"], msg["kind"])
                elif (
                    not requests
                    and key in pending
                    and msg["kind"] in ("search_result", "classify_result", "error")
                ):
                    query, kind = pending.pop(key)
                    result = msg.get(
                        "modality",
                        [Path(h["doc_id"]).name for h in msg.get("hits", [])],
                    )
                    print(json.dumps([query, kind, result]), file=trace, flush=True)
                writer.write(header + body)
                await writer.drain()

        try:
            await asyncio.gather(
                relay(daemon[0], worker[1], True),
                relay(worker[0], daemon[1], False),
            )
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            worker[1].close()
            daemon[1].close()
            await proc.wait()


if __name__ == "__main__":
    with open(os.environ["QUALITY_TRACE"], "a") as trace:
        asyncio.run(main(trace))
