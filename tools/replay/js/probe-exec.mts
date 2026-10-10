import { run } from "../src/lib/exec.ts";
console.log(JSON.stringify(await run("definitely-not-a-binary-xyz", ["-n","x"])));
console.log(JSON.stringify(await run("sh", ["-c","echo 'sudo: a password is required' >&2; exit 1"])));
console.log(JSON.stringify(await run("sh", ["-c","exit 3"])));
