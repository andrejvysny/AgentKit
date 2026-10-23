import { createLiveFixture } from "../../../../packages/transport-http/tests/support/fixture.ts";
const fixture = await createLiveFixture();
const server = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  fetch: fixture.handler,
});
console.log(server.url.href);
