// The Buzz tools as an MCP server inside the host process: answers one
// JSON-RPC message at a time, as Claude Code sends them over its stream-json
// pipe to an in-process (`sdk`) server. Holds no state between messages.
import type { AgentAttention } from "../features/agents2/service";
import {
  ATTENTION_TOOLS,
  callAttentionTool,
  isAttentionTool,
} from "./attention";
import type { BuzzClient, Context } from "./client";
import { callTool, TOOLS } from "./tools";

type Message = Readonly<{
  id?: string | number | null;
  method?: string;
  params?: Readonly<Record<string, unknown>>;
}>;

/** The attention tool set for one session. `offered` decides the tool list,
 * which a session reads once, so it is fixed when the session starts; `api` is
 * the agent's attention once a delivery has given the session a handle. */
export type AttentionTools = Readonly<{
  offered: boolean;
  api?: AgentAttention;
}>;

/** Answers `input`. Without a client, tools are listed but cannot be called.
 * Attention tools are listed only when `attention` offers them. */
export async function respond(
  client: BuzzClient | undefined,
  context: Context,
  input: unknown,
  attention?: AttentionTools,
) {
  const { id, method, params = {} } = (input ?? {}) as Message;
  // A notification needs no answer, but the pipe expects one.
  if (id === undefined || id === null)
    return { jsonrpc: "2.0", id: 0, result: {} };
  const result = (value: unknown) => ({ jsonrpc: "2.0", id, result: value });
  switch (method) {
    case "initialize":
      return result({
        protocolVersion: params.protocolVersion ?? "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "buzz", version: "1" },
      });
    case "ping":
      return result({});
    case "tools/list":
      return result({
        tools: attention?.offered ? [...TOOLS, ...ATTENTION_TOOLS] : TOOLS,
      });
    case "tools/call":
      try {
        const name = String(params.name);
        const args = (params.arguments ?? {}) as Record<string, unknown>;
        let text: string;
        if (attention?.offered && isAttentionTool(name)) {
          const api = attention.api;
          if (!api) throw new Error("Buzz is not ready yet. Try again.");
          // The owner can turn attention off while this session runs.
          if (!api.enabled())
            throw new Error(
              `Attention is off for you: your owner turned it off, so these tools do nothing now.\n(classifier: ${api.classifier()})`,
            );
          text = await callAttentionTool(api, name, args);
        } else {
          if (!client) throw new Error("Buzz is not ready yet. Try again.");
          text = await callTool(client, context, name, args);
        }
        return result({ content: [{ type: "text", text }] });
      } catch (error) {
        return result({
          content: [{ type: "text", text: errorText(error) }],
          isError: true,
        });
      }
    default:
      return {
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: `No method ${String(method)}` },
      };
  }
}

export const errorText = (error: unknown) =>
  error instanceof Error ? error.message : String(error);
