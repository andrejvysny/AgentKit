/**
 * Public barrel for `@agentkit/react`: headless hooks over `@agentkit/client`.
 *
 * HEADLESS MEANS HEADLESS. There is one component in this package — the
 * provider that carries the client and the invalidation bus — and it renders
 * nothing. No message list, no bubble, no composer, no spinner, no class name,
 * no CSS import. What every consumer of this framework has in common is the
 * PROTOCOL (submit, stream, reconcile, branch, approve); what none of them
 * share is the interface, and a component library here would be a design
 * system three applications have to fight rather than a dependency they can
 * use.
 *
 * `react` is a PEER dependency, `>=18`, and nothing here imports `react-dom`:
 * these hooks work in a renderer, in React Native, in a custom reconciler, and
 * they do not touch `window` — streaming starts in an effect, so a server
 * render sees the initial state and nothing more.
 */
export {
  AgentKitProvider,
  useAgentKitClient,
  useAgentKitContext,
  useAgentKitEmitter,
  type AgentKitContextValue,
  type AgentKitProviderProps,
} from "./context.js";
export {
  chatTopic,
  createChangeEmitter,
  type ChangeEmitter,
  type ChangeEvent,
  type ChangeListener,
} from "./emitter.js";
export {
  DEFAULT_MAX_PAGES,
  loadActivePath,
  type PagingOptions,
} from "./messages.js";
export {
  useBranches,
  type BranchState,
  type UseBranchesOptions,
  type UseBranchesResult,
} from "./use-branches.js";
export {
  useChat,
  type ChatState,
  type ChatStatus,
  type SubmitOptions,
  type UseChatOptions,
  type UseChatResult,
} from "./use-chat.js";
export {
  useProposals,
  type ProposalsState,
  type UseProposalsOptions,
  type UseProposalsResult,
} from "./use-proposals.js";
export {
  useProviders,
  type ProvidersState,
  type UseProvidersOptions,
  type UseProvidersResult,
} from "./use-providers.js";
export {
  useRun,
  type RunState,
  type UseRunOptions,
  type UseRunResult,
} from "./use-run.js";
