/**
 * Which resource each contract route is about, and how a principal becomes an
 * {@link AuthorizationSubject}.
 *
 * The table lives here rather than inline in the handler because it is the part
 * of authorization this adapter actually decides. `AuthorizationPort` answers
 * "may this actor touch that resource?"; naming *which* resource a URL is about
 * is not the host's job — the host never sees the URL — and getting it wrong is
 * silent: an authorizer handed the wrong id says yes to the wrong question.
 *
 * Keyed by {@link RestOperation} and `satisfies` a total record, so a route
 * added to `REST_ROUTES` fails this package's compile until it is given a
 * resource. That is the same guarantee `HANDLERS` gives for serving the route,
 * and for the same reason: a new route that quietly authorized as nothing would
 * be a hole nobody would find by reading a diff.
 */
import type { RestOperation } from "@agentkit/contracts";
import type {
  AuthorizationResource,
  AuthorizationSubject,
} from "@agentkit/host";
import type { RouteParams } from "./router.js";

/** The action verb passed to the port: reads and everything else. */
export type RestAction = "read" | "write";

/**
 * Builds the resource one request is about, from that route's path parameters
 * — and, for the one route whose subject is in the query string, from the URL.
 *
 * `null` means the route is never authorized — see {@link getVersion} below.
 *
 * Almost every entry below ignores the second argument, which is why it is a
 * positional parameter rather than a named field: a one-argument function still
 * satisfies this type, so adding the URL cost the table nothing.
 */
export type RouteResource =
  | ((params: RouteParams, url: URL) => AuthorizationResource)
  | null;

const chat = (params: RouteParams): AuthorizationResource => ({
  kind: "chat",
  ...(params["chatId"] === undefined ? {} : { id: params["chatId"] }),
});

const message = (params: RouteParams): AuthorizationResource => ({
  kind: "message",
  ...(params["messageId"] === undefined ? {} : { id: params["messageId"] }),
});

const run = (params: RouteParams): AuthorizationResource => ({
  kind: "run",
  ...(params["runId"] === undefined ? {} : { id: params["runId"] }),
});

const proposal = (params: RouteParams): AuthorizationResource => ({
  kind: "proposal",
  ...(params["proposalId"] === undefined ? {} : { id: params["proposalId"] }),
});

const provider = (params: RouteParams): AuthorizationResource => ({
  kind: "provider",
  ...(params["providerId"] === undefined ? {} : { id: params["providerId"] }),
});

const settings = (): AuthorizationResource => ({ kind: "settings" });

/**
 * The write policy of ONE CHAT — `kind: "policy"`, `id` the chat.
 *
 * All three allowance routes are nested under `/v1/chats/:chatId`, so the chat
 * whose consent is at stake is always in the path, and an authorizer can gate a
 * grant per conversation without reading a body it is never handed. The `kind`
 * stays `policy` rather than `chat` because that is what the request is about:
 * a host that answers "may this subject touch that chat?" and a host that
 * answers "may this subject grant standing writes in it?" are answering two
 * different questions, and collapsing them would make the second one
 * unaskable.
 *
 * `revokeAllowance` also carries an `allowanceId`, which is deliberately NOT
 * the id here: an authorizer handed a key it has never seen can decide
 * nothing, and the chat is the scope that consent actually belongs to.
 */
const policy = (params: RouteParams): AuthorizationResource => ({
  kind: "policy",
  ...(params["chatId"] === undefined ? {} : { id: params["chatId"] }),
});

const mcpConfig = (params: RouteParams): AuthorizationResource => ({
  kind: "mcp_config",
  ...(params["serverId"] === undefined ? {} : { id: params["serverId"] }),
});

/**
 * `searchMessages` — the one route whose subject is in the QUERY STRING.
 *
 * `?chatId=` scopes the search to one conversation, and when it is there the
 * request is about that chat and must be authorized as one. Without it the
 * search spans every chat in the store, and the honest resource is the
 * unscoped `{ kind: "chat" }` — an authorizer that denies it denies exactly the
 * cross-conversation search, which is the decision it should be making. Reading
 * the id from the path is impossible here, and inventing a scope would be
 * worse: a host that only ever expects scoped searches must be able to see that
 * an unscoped one arrived.
 */
const searchScope = (_params: RouteParams, url: URL): AuthorizationResource => {
  const chatId = url.searchParams.get("chatId");
  return {
    kind: "chat",
    ...(chatId === null || chatId === "" ? {} : { id: chatId }),
  };
};

/**
 * Route → resource.
 *
 * The `kind` is the kind of the RESOURCE the route is about, and the `id` comes
 * from the path — never from a body, and (except for `searchMessages`) never
 * from the query. Two consequences worth spelling out, because both are places
 * an obvious-looking mapping would be a bug:
 *
 * - `activateBranch` and `listSiblings` are rooted at `/v1/messages/:messageId`
 *   and carry no chat id. They are `kind: "message"`, not `kind: "chat"` with a
 *   message id in `id` — an authorizer that looked that id up as a chat would
 *   find nothing and deny (or worse, throw) on a request that is perfectly
 *   legitimate. A host that scopes on conversations resolves message → chat
 *   itself; it is the only side that can.
 * - `listToolEvents` is `/v1/chats/:chatId/tool-events`. It *derives* its answer
 *   from runs, but it names a chat and is authorized as one, for the same
 *   reason: `{ kind: "run", id: <a chat id> }` would be a lie the host cannot
 *   detect.
 *
 * Three routes carry TWO ids or NONE, and each is called out where it is
 * defined: `regenerateMessage` names a chat and a message and is authorized as
 * the chat it writes to; `revokeAllowance` names a chat and an allowance key
 * and is authorized as the chat's POLICY; and `searchMessages` takes its scope
 * from `?chatId=` because there is no path id to take it from. Everything else
 * is the single id its path carries.
 *
 * `getVersion` is the one route that is never authorized. It reads two
 * constants and the host's own `packages` map — no store, no user data — and it
 * is what a client calls to discover whether it can talk to this server at all.
 * Gating it means a client that fails authorization cannot even learn what
 * contract version it failed against.
 */
export const RESOURCE_BY_OPERATION = {
  createChat: chat,
  listChats: chat,
  getChat: chat,
  updateChat: chat,
  deleteChat: chat,
  listMessages: chat,
  submitMessage: chat,
  // `chat`, not `message`, even though the path carries both ids: a regenerate
  // WRITES to the conversation — a new branch, a new run — and the message id
  // names a position inside it rather than the thing being touched. The rule
  // above still holds, because the chat id is one of the ids the path carries;
  // the choice here is which of the two the request is ABOUT.
  regenerateMessage: chat,
  forkChat: chat,
  searchMessages: searchScope,

  activateBranch: message,
  listSiblings: message,

  getRun: run,
  streamRun: run,
  cancelRun: run,

  listToolEvents: chat,

  listProposals: chat,
  approveProposal: proposal,
  rejectProposal: proposal,
  applyProposal: proposal,

  listProviders: provider,
  createProvider: provider,
  updateProvider: provider,
  deleteProvider: provider,
  listModels: provider,
  refreshProviderModels: provider,
  testProvider: provider,

  getSettings: settings,
  updateSettings: settings,

  listAllowances: policy,
  grantAllowance: policy,
  revokeAllowance: policy,

  listMcpServers: mcpConfig,
  createMcpServer: mcpConfig,
  updateMcpServer: mcpConfig,
  deleteMcpServer: mcpConfig,

  listTools: () => ({ kind: "tools" }),

  getVersion: null,
} satisfies Readonly<Record<RestOperation, RouteResource>>;

/** The resource this operation is about, or `null` when it is never authorized. */
export function resourceForOperation(
  operation: RestOperation,
  params: RouteParams,
  url: URL,
): AuthorizationResource | null {
  const resolve: RouteResource = RESOURCE_BY_OPERATION[operation];
  return resolve === null ? null : resolve(params, url);
}

/**
 * Everything that is not a GET is a `write`.
 *
 * Coarser than the port's `action` field allows (it documents verbs like
 * `chat.submit`), and deliberately so: this adapter knows the HTTP method and
 * nothing else about intent, and a finer vocabulary invented here would be one
 * this package would then have to keep in step with the route table forever.
 * `POST /v1/runs/:runId/cancel` is a write in every sense that matters — it
 * changes state — even though it writes no field a client named.
 */
export function actionForMethod(method: string): RestAction {
  return method === "GET" ? "read" : "write";
}

/**
 * The `authenticate` result, as the port's subject.
 *
 * An object passes through untouched, so a host that already returns
 * `{ userId, roles }` needs no adapter. Anything else — the bare token or user
 * id a simpler host returns — is carried under `metadata.principal` rather than
 * guessed at: this adapter has no way to know whether a string is a user id, a
 * session id or an API key name, and putting it in `userId` would make an
 * authorizer's `subject.userId` mean three different things.
 */
export function subjectForPrincipal(principal: unknown): AuthorizationSubject {
  if (principal !== null && typeof principal === "object") {
    return principal as AuthorizationSubject;
  }
  if (principal === undefined || principal === null) return {};
  return { metadata: { principal } };
}
