import { invoke, isTauri } from "@tauri-apps/api/core";
import { eventsDto, type RelayEvent } from "../relay/events";

/** A plugin agent's identity as native custody reports it; never the key. The
 * saved identities are the agent list. */
export type AgentIdentity = Readonly<{
  pubkey: string;
  /** The canonical `wss://` community origin it was created for. */
  relay: string;
  owner: string;
  /** The agent type's key, `pluginId/typeId`, fixed at create. */
  type: string;
  name: string;
  /** Its key is gone; its community still has to drop it from its channels and
   * archive it, after which `forget` removes it. */
  deleted: boolean;
}>;
export type AgentEventTemplate = Readonly<{
  kind: number;
  content: string;
  tags?: readonly (readonly string[])[];
}>;
/** The community's description of an uploaded blob. */
export type AgentUpload = Readonly<{
  url: string;
  sha256: string;
  size: number;
  type: string;
  dim?: string;
  blurhash?: string;
  thumb?: string;
  duration?: number;
}>;
export type AgentsNative = {
  list(): Promise<readonly AgentIdentity[]>;
  /** Generates a key, has the signed-in owner attest it, and saves both. */
  create(
    destination: string,
    owner: string,
    type: string,
    name: string,
  ): Promise<AgentIdentity>;
  rename(pubkey: string, name: string): Promise<void>;
  /** Deletes the key and marks the identity deleted. */
  remove(pubkey: string): Promise<void>;
  /** Drops a deleted identity. */
  forget(pubkey: string): Promise<void>;
  /** Signs one event as the agent (deletions, reactions, messages, edits,
   * canvases and DM opens only) with its owner attestation and posts it to the
   * agent's community. */
  publish(pubkey: string, event: AgentEventTemplate): Promise<RelayEvent>;
  /** Reads the agent's community as the agent: one to eight filters. */
  query(pubkey: string, filters: readonly object[]): Promise<RelayEvent[]>;
  /** Uploads base64 `data` (an image or MP4) as the agent. */
  upload(pubkey: string, data: string, mime: string): Promise<AgentUpload>;
  /** Writes memory entry `slug`, encrypted to the owner, newer than `after`. */
  remember(
    pubkey: string,
    slug: string,
    body: string,
    after: number,
  ): Promise<RelayEvent>;
  /** Publishes its name as its profile unless the community already has it. */
  publishProfile(pubkey: string): Promise<void>;
  /** Whether this app runs the agent: every copy of the app on this machine
   * shares the agent list, and one holds each agent until it exits. A dev build
   * lets go while an installed app is running. */
  claim(pubkey: string): Promise<boolean>;
  /** Lets another copy run it. */
  release(pubkey: string): Promise<void>;
};

export const nativeAgents = (): AgentsNative | undefined =>
  isTauri()
    ? {
        list: () => invoke("app_agent_list"),
        create: (destination, owner, agentType, name) =>
          invoke("app_agent_create", { destination, owner, agentType, name }),
        rename: (pubkey, name) => invoke("app_agent_rename", { pubkey, name }),
        remove: (pubkey) => invoke("app_agent_delete", { pubkey }),
        forget: (pubkey) => invoke("app_agent_forget", { pubkey }),
        publish: (pubkey, event) =>
          invoke("app_agent_publish", {
            pubkey,
            event: {
              kind: event.kind,
              content: event.content,
              tags: event.tags ?? [],
            },
          }),
        query: async (pubkey, filters) =>
          eventsDto(await invoke("app_agent_query", { pubkey, filters })),
        upload: (pubkey, data, mime) =>
          invoke("app_agent_upload", { pubkey, data, mime }),
        remember: (pubkey, slug, body, after) =>
          invoke("app_agent_remember", { pubkey, slug, body, after }),
        publishProfile: (pubkey) =>
          invoke("app_agent_publish_profile", { pubkey }),
        claim: (pubkey) => invoke("app_agent_claim", { pubkey }),
        release: (pubkey) => invoke("app_agent_release", { pubkey }),
      }
    : undefined;
