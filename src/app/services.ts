// FOUNDATION: Compose the bundled distribution, plugin runtime, and services here.
import {
  createIdentity,
  nativeIdentityEnabled,
} from "../features/identity/service";
import { AgentSecurityService } from "../features/agents/security";
import { SettingsCardsService } from "../features/settings/service";
import { TemplateProvidersService } from "../features/channel-templates/provider";
import { IdentityNamesService } from "../features/identity-names/service";
import { bindAgentMentions } from "../features/agents/mention-wake";
import { provideAgentControl } from "../features/agents/control-service";
import { HostService } from "../features/host/service";
import { bindUnreadIndicator } from "../features/notifications/indicator-unread";
import { provideNavigation } from "../features/navigation/service";
import { bindDeepLinks } from "../features/navigation/deep-links";
import { bindSearchUsage } from "../features/search/usage";
import { communityDestination } from "../features/communities/destination";
import { NotificationsService } from "../features/notifications/service";
import {
  bindMessageNotifications,
  notificationAuthorized,
} from "../features/notifications/messages";
import { AccountActionsService } from "../features/account-actions/service";
import { ShortcutsService } from "../features/shortcuts/service";
import { Agents2Service } from "../features/agents2/service";
import { createShortcutBindings } from "../features/shortcuts/preferences";
import { ConversationService } from "../features/conversation/service";
import { createAppearance } from "../shared/theme/service";
import { createCommunities } from "../features/communities/service";
import { createInviteIntent } from "../features/communities/invite-intent";
import { connectNativeTransport } from "../features/relay/native";
import { createUpdates } from "../features/updates/updates";
import { PanelsService } from "../features/panels/service";
import { Context } from "@deepseek-ai/cordis";
import { BrowserService } from "../features/browser/service";
import { PagesService } from "../features/pages/service";
import { bundledPlugins } from "../bundled";
import { createPluginManager } from "../plugins/manager";
import { withTimeout } from "../plugins/timeout";

export function createServices() {
  const appearance = createAppearance();
  const shortcutBindings = createShortcutBindings();
  const updates = createUpdates();
  const ctx = new Context();
  const plugins = createPluginManager(ctx, {
    bundled: bundledPlugins,
  });
  new HostService(ctx, plugins);
  const agentControl = provideAgentControl(ctx);
  new AgentSecurityService(ctx);
  const navigationHost = provideNavigation(ctx);
  const navigation = navigationHost.navigation;
  const browser = new BrowserService(ctx);
  const shortcuts = new ShortcutsService(ctx, undefined, shortcutBindings);
  const pages = new PagesService(ctx);
  const panels = new PanelsService(ctx);
  const accountActions = new AccountActionsService(ctx);
  const conversation = new ConversationService(ctx);
  const settingsCards = new SettingsCardsService(ctx);
  const channelTemplates = new TemplateProvidersService(ctx);
  const identityNames = new IdentityNamesService(ctx, agentControl);
  const identity = nativeIdentityEnabled() ? createIdentity() : undefined;
  const communities = createCommunities(
    ctx,
    import.meta.env.VITE_BUZZ_LIVE === "1",
    identityNames,
    import.meta.env.VITE_BUZZ_OPEN_RELAY ?? "",
    agentControl,
    identity?.ready,
    identity ? connectNativeTransport : undefined,
  );
  ctx.provide("communityReader", {
    snapshot: communities.snapshot,
    subscribe: communities.subscribe,
  });
  const relay = communities.relay;
  const agents2 = new Agents2Service(ctx, relay);
  ctx.effect(() => bindAgentMentions(agentControl, communities));
  const notifications = new NotificationsService(
    ctx,
    navigation,
    undefined,
    undefined,
    (target) => notificationAuthorized(communities, target),
  );
  ctx.effect(() => bindMessageNotifications(notifications, communities));
  const invites = createInviteIntent();
  // OS deep links; a no-op in the browser build. Invite admission remains in
  // the join dialog, never in navigation or the native queue.
  ctx.effect(() =>
    bindDeepLinks({ ...navigationHost, invite: invites.open }, communities),
  );
  // Search ranks places by how often and how recently they were opened.
  ctx.effect(() =>
    bindSearchUsage(navigation, () => {
      const { selected, viewer } = communities.snapshot();
      return selected && viewer
        ? { viewer, communityOrigin: communityDestination(selected).url }
        : undefined;
    }),
  );
  if (notifications.indicator.available)
    ctx.effect(() =>
      bindUnreadIndicator(communities, notifications.indicator.setUnread),
    );
  let disposal: Promise<void> | undefined;
  return {
    identity,
    agentControl,
    browser,
    notifications,
    navigation,
    navigationHost,
    shortcuts,
    agents2,
    accountActions,
    shortcutBindings,
    conversation,
    settingsCards,
    channelTemplates,
    pages,
    panels,
    plugins,
    relay,
    communities,
    invites,
    appearance,
    updates,
    dispose() {
      identity?.dispose();
      appearance.dispose();
      updates.dispose();
      shortcutBindings.dispose();
      // Start root cancellation without waiting for plugin-owned cleanup. Cordis
      // starts sibling effects independently; the runtime still owns replacement
      // barriers. A timeout reports incomplete cleanup, never successful disposal.
      disposal ??= withTimeout(
        Promise.all([plugins.dispose(), ctx.fiber.dispose()]),
        "App cleanup timed out; restart the app",
      ).then(() => {});
      return disposal;
    },
  };
}

export type AppServices = ReturnType<typeof createServices>;
