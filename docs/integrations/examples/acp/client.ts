import { spawn } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { Readable, Writable } from "node:stream";
import * as acp from "@agentclientprotocol/sdk";

const cwd = process.env.ACP_WORKSPACE;
if (!cwd || !process.env.EGO_HOME || !process.env.EGO_BIN) {
  throw new Error("Run through isolated.sh");
}
const child = spawn(process.env.EGO_BIN, [
  "acp", "--bare", "--no-tools", "--profile", "example", "-C", cwd,
], { stdio: ["pipe", "pipe", "inherit"] });
const exited = once(child, "exit");
const deadline = setTimeout(() => child.kill("SIGKILL"), 20000);
try {
  await acp.client({ name: "ego-doc-example" })
    .onRequest(acp.methods.client.session.requestPermission, () => {
      // Fail closed. A UI can instead select an optionId from the offered list.
      return { outcome: { outcome: "cancelled" } };
    })
    .onNotification(acp.methods.client.session.update, ({ params }) => {
      const update = params.update;
      console.log("update", update.sessionUpdate);
      if (update.sessionUpdate === "agent_message_chunk" && update.content.type === "text") {
        process.stdout.write(update.content.text);
      }
    })
    .connectWith(acp.ndJsonStream(Writable.toWeb(child.stdin), Readable.toWeb(child.stdout)), async (ctx) => {
      const init = await ctx.request("initialize", { protocolVersion: 1, clientCapabilities: {} });
      if (init.protocolVersion !== 1) throw new Error("Expected ACP v1");
      console.log("initialize", init.protocolVersion);
      const caps = init.agentCapabilities;
      const session = await ctx.request("session/new", {
        cwd, mcpServers: [], _meta: { ego: { bare: true } },
      });
      const sessionId = session.sessionId;
      console.log("session/new ok");
      // Request extensions use their full wire name; no SDK fork is needed.
      const hold = caps._meta?.ego as { hold?: { version?: number } } | undefined;
      if (hold?.hold?.version === 1) {
        const result = await ctx.request<{ state: string }>("_ego/pause", {
          v: 1, sessionId, requestId: randomUUID(),
        });
        console.log("_ego/pause", result.state);
        await ctx.request("_ego/resume", { v: 1, sessionId, requestId: randomUUID() });
      }
      // No daemon is listening at port 1: exercise error settlement, not generation.
      try {
        await ctx.request("session/prompt", { sessionId, prompt: [{ type: "text", text: "Hello" }] });
        throw new Error("Unexpected successful generation in isolated mode");
      } catch (error) {
        if (!(error instanceof acp.RequestError)) throw error;
        console.log("session/prompt refused", error.code);
      }
      await ctx.notify("session/cancel", { sessionId });
      console.log("session/cancel sent (idle)");
      if (caps.loadSession) {
        await ctx.request("session/load", { sessionId, cwd, mcpServers: [] });
        console.log("session/load ok");
      }
      if (caps.sessionCapabilities?.resume) {
        await ctx.request("session/resume", { sessionId, cwd, mcpServers: [] });
        console.log("session/resume ok");
      }
      if (caps.sessionCapabilities?.fork) {
        // Draft upstream surface: use the generic request API at this pin.
        await ctx.request("session/fork", { sessionId, cwd, mcpServers: [] });
        console.log("session/fork ok");
      }
      child.stdin.end(); // Let the stdio server exit before SDK transport teardown.
    });
} finally {
  clearTimeout(deadline);
  if (child.exitCode === null && child.signalCode === null) child.kill();
  await exited;
}
