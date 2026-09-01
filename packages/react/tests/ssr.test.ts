/**
 * Server rendering: the hooks must produce their initial state and NOTHING
 * else.
 *
 * The rule they have to keep is narrow and easy to break by accident — no
 * `window`, no `document`, and no request during render. Every read here starts
 * in a `useEffect`, which `renderToStaticMarkup` never runs, so a server render
 * is a pure function of the props. The assertion is on the `fetch` counter: a
 * hook that kicked its load off in the render body would show up as a request
 * from a render that is supposed to be free of them.
 */
import "./support/dom.js";
import { createAgentKitClient, type FetchLike } from "@agentkit/client";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  AgentKitProvider,
  useBranches,
  useChat,
  useProposals,
  useProviders,
  useRun,
} from "../src/index.js";
import {
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;

beforeEach(async () => {
  server = await startTestServer();
});

afterEach(async () => {
  await server.stop();
});

describe("server rendering", () => {
  test("every hook renders its initial state without a single request", () => {
    let calls = 0;
    const counting: FetchLike = async (url, init) => {
      calls += 1;
      return fetch(url, init);
    };
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: counting,
    });

    const seen: string[] = [];
    function Probe(): null {
      const chat = useChat(TEST_CHAT_ID);
      const run = useRun("run-1");
      const branches = useBranches("msg-1");
      const proposals = useProposals(TEST_CHAT_ID);
      const providers = useProviders();
      seen.push(
        JSON.stringify({
          messages: chat.messages.length,
          status: chat.status,
          events: run.events.length,
          count: branches.count,
          proposals: proposals.proposals.length,
          providers: providers.providers.length,
        }),
      );
      return null;
    }

    const html = renderToStaticMarkup(
      createElement(AgentKitProvider, { client }, createElement(Probe)),
    );

    expect(html).toBe("");
    expect(calls).toBe(0);
    expect(seen).toEqual([
      JSON.stringify({
        messages: 0,
        status: "idle",
        events: 0,
        count: 0,
        proposals: 0,
        providers: 0,
      }),
    ]);
  });
});
