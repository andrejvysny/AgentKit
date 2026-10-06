import { buildNodeHost } from "./node-host.mjs";

const app = await buildNodeHost(
  process.env.AGENTKIT_DB ?? "./agentkit-node.sqlite",
);
console.log(`Local fake-provider host: ${app.origin}`);
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.once(signal, () => {
    void app.stop().catch((error) => {
      console.error(error);
      process.exitCode = 1;
    });
  });
}
