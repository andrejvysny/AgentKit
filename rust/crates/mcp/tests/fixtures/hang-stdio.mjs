import { writeFileSync } from "node:fs";

writeFileSync(process.env.PID_FILE, String(process.pid));
process.stdin.resume();
setInterval(() => {}, 1000);
