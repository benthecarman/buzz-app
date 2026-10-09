// FOUNDATION: Type-only author entry. Runtime capabilities come from injected ctx.
export type { PluginManifest, PluginModule } from "./api";
export type { SettingsCard, SettingsCards } from "../features/settings/service";
export type {
  TemplateDraft,
  TemplateEditorProps,
  GroupDefaultProps,
  SaveTemplateProps,
  TemplateProvider,
  TemplateProviders,
} from "../features/channel-templates/provider";
export type { Context } from "@deepseek-ai/cordis";
export type {
  Host,
  HostRequest,
  HostResponse,
  HostPlugins,
  HostPluginAction,
} from "../features/host/service";
export type {
  Catalog,
  PluginInfo,
  ImportPreview,
} from "./types";
export type {
  IdentityNames,
  NamingPolicy,
} from "../features/identity-names/service";
export type { NamingIdentity } from "../features/identity-names/policy";
export type { Browser, BrowserViewProps } from "../features/browser/api";
export type { Page, Pages } from "../features/pages/service";
export type {
  Panel,
  Panels,
  ChannelPanelContext,
  ChannelLauncherProps,
} from "../features/panels/service";
export type { Conversation } from "../features/conversation/service";
export type {
  ComposerObservation,
  CompletionContext,
  CompletionQuery,
  CompletionEdit,
  CompletionSuggestion,
  CompletionResult,
  ComposerCompletionProps,
  ComposerCompletion,
  ComposerAccessory,
  ComposerAccessoryProps,
  ComposerResource,
  ComposerCaptureProps,
  AttachmentRenderer,
  AttachmentRendererProps,
  ComposerToolProps,
  ComposerTool,
  InlineContent,
  InlineRange,
  InlineRenderer,
  LinkRenderer,
  MessageRenderer,
} from "../features/conversation/contracts";
export type { RelayData, RelaySnapshot } from "../features/relay/service";

export type {
  Shortcuts,
  Shortcut,
  KeyBinding,
  RegisteredShortcut,
} from "../features/shortcuts/service";

export type {
  UnreadCapability,
  UnreadSnapshot,
  ThreadActivityItem,
  ThreadActivitySnapshot,
  ReadingHandle,
} from "../features/relay/unread";
export type { ReadTarget } from "../features/relay/read-state-model";
export type {
  ReadMutationResult,
  ReadSyncSnapshot,
} from "../features/relay/read-state";

export type {
  Navigation,
  NavigationSnapshot,
  OpenResult,
  OpenFailure,
} from "../features/navigation/controller";
export type {
  OpenTarget,
  NavigationScope,
  JsonValue,
} from "../features/navigation/targets";
export type { PageNavigation } from "../features/navigation/service";

export type {
  Notifications,
  NotificationInput,
  NotificationCategoryDescriptor,
} from "../features/notifications/service";

export type {
  AgentControl,
  AgentControlState,
  AgentView,
  AgentEdit,
  AgentAction,
  ControlSnapshot,
  AgentImportPreview,
  ImportSource,
} from "../features/agents/control";

export type {
  AgentSecurity,
  SecurityBinding,
  SecuritySnapshot,
  SecurityProvider,
} from "../features/agents/security";

export type { VoiceRecording } from "../features/relay/voice-media";
