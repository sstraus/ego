"""ACP v1 client using agent-client-protocol==0.12.1; run via isolated.sh."""

import asyncio
import os
import uuid

from acp import Client, RequestError, connect_to_agent
from acp.schema import (
    ClientCapabilities,
    RequestPermissionResponse,
    TextContentBlock,
)


class Host(Client):
    async def request_permission(self, session_id, tool_call, options, **kwargs):
        # A real UI can return a selected option_id from options. Never choose
        # the first option automatically: it can grant persistent authority.
        return RequestPermissionResponse(outcome={"outcome": "cancelled"})

    async def session_update(self, session_id, update, **kwargs):
        print("update", update.session_update)
        if update.session_update == "agent_message_chunk":
            if isinstance(update.content, TextContentBlock):
                print(update.content.text, end="", flush=True)


async def main():
    cwd = os.environ["ACP_WORKSPACE"]
    if not os.environ.get("EGO_HOME"):
        raise RuntimeError("Run through isolated.sh")
    proc = await asyncio.create_subprocess_exec(
        os.environ["EGO_BIN"], "acp", "--bare", "--no-tools",
        "--profile", "example", "-C", cwd,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
    )
    conn = connect_to_agent(Host(), proc.stdin, proc.stdout)
    try:
        async with asyncio.timeout(20):
            init = await conn.initialize(protocol_version=1, client_capabilities=ClientCapabilities())
            if init.protocol_version != 1:
                raise RuntimeError("Expected ACP v1")
            print("initialize", init.protocol_version)
            caps = init.agent_capabilities
            # Keyword extras are placed inside _meta by this SDK, not at root.
            session = await conn.new_session(cwd=cwd, mcp_servers=[], ego={"bare": True})
            sid = session.session_id
            print("session/new ok")
            meta = caps.field_meta or {}
            if meta.get("ego", {}).get("hold", {}).get("version") == 1:
                # Python prepends ONE underscore: pass ego/pause, NOT _ego/pause.
                result = await conn.ext_method("ego/pause", {
                    "v": 1, "sessionId": sid, "requestId": str(uuid.uuid4()),
                })
                print("_ego/pause", result["state"])
                await conn.ext_method("ego/resume", {
                    "v": 1, "sessionId": sid, "requestId": str(uuid.uuid4()),
                })
            try:
                await conn.prompt(session_id=sid, prompt=[TextContentBlock(type="text", text="Hello")])
            except RequestError as error:
                print("session/prompt refused", error.code)
            else:
                raise RuntimeError("Unexpected successful generation in isolated mode")
            await conn.cancel(session_id=sid)
            print("session/cancel sent (idle)")
            if caps.load_session:
                await conn.load_session(session_id=sid, cwd=cwd, mcp_servers=[])
                print("session/load ok")
            lifecycle = caps.session_capabilities
            if lifecycle and lifecycle.resume is not None:
                await conn.resume_session(session_id=sid, cwd=cwd, mcp_servers=[])
                print("session/resume ok")
            if lifecycle and lifecycle.fork is not None:
                await conn.fork_session(session_id=sid, cwd=cwd, mcp_servers=[])
                print("session/fork ok")
    finally:
        await conn.close()
        if proc.returncode is None:
            proc.terminate()
        try:
            await asyncio.wait_for(proc.wait(), 3)
        except TimeoutError:
            proc.kill()
            await proc.wait()


if __name__ == "__main__":
    asyncio.run(main())
