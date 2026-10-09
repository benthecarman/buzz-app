// FOUNDATION: Public plugin contract. Changes affect independently built plugins.
import type { Context, Inject } from "@deepseek-ai/cordis";
import type * as React from "react";

// Metadata is readable without executing the plugin.
export type PluginManifest = Readonly<{
  id: string;
  name: string;
  apiVersion: 1;
  host?: Readonly<{
    commands?: readonly Readonly<{
      id: string;
      program: string;
      args: readonly string[];
      /** Maximum UTF-8 stdout bytes; defaults to 4096, bounded at 1048576. */
      maxOutputBytes?: number;
    }>[];
    networkOrigins?: readonly string[];
    /** Programs the plugin may start with `host.spawn`; the caller's
     * arguments follow the declared ones. */
    processes?: readonly Readonly<{
      id: string;
      program: string;
      args?: readonly string[];
    }>[];
    /** Manage plugins through `ctx.host.plugins`. */
    plugins?: boolean;
  }>;
}>;
// Module evaluation must be pure. apply owns resources through ctx.effect.
// A plugin may contribute to several surfaces, or provide services without UI.
export type PluginModule = {
  // Cordis starts apply only when these services are available.
  inject?: Inject;
  apply: (ctx: Context) => void | Promise<void>;
};

// Installation identity follows child Cordis scopes so services can own contributions.
declare module "@deepseek-ai/cordis" {
  interface Context {
    react: typeof React;
    readonly pluginOwner?: Readonly<{ id: string; revision: string }>;
  }
}
