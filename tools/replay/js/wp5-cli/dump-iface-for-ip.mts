// Phase 4 WP5 (CF22): `ifaceForIp` / `ipv4Of` (src/hardware/network.ts:68-75,
// :154-163) over recorded `os.networkInterfaces()` snapshots → the REAL TS
// functions with node:os patched (syncBuiltinESMExports) →
// fixtures/network/iface_for_ip.json.
//
//   tools/replay/js/run-tsx.sh <plans>/phase-4/evidence/wp5-tools/dump-iface-for-ip.mts > fixtures/network/iface_for_ip.json
import os from "node:os";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";

type Addr = { family: string | number; address: string; internal: boolean };
let snap: Record<string, Addr[]> = {};
(os as unknown as { networkInterfaces: () => unknown }).networkInterfaces = () => snap;
syncBuiltinESMExports();
const { ifaceForIp, ipv4Of } = await import(join(process.cwd(), "src/hardware/network.ts"));

const v4 = (address: string, internal = false): Addr => ({ family: "IPv4", address, internal });
const v6 = (address: string, internal = false): Addr => ({ family: "IPv6", address, internal });
// Bench-SHAPED snapshots (CM5: lo, usb0 = SIM7600 RNDIS, wlan0 = Wi-Fi on the
// GL router at 192.168.8.203 — addresses from the plan/bench notes, not a live
// read; WP5 never touches the bench) plus edge cases.
const SNAPSHOTS: { name: string; ifs: Record<string, Addr[]> }[] = [
  { name: "bench wifi+lte", ifs: { lo: [v4("127.0.0.1", true), v6("::1", true)], usb0: [v4("192.168.225.37"), v6("fe80::a8f2:1ff:fe00:1")], wlan0: [v4("192.168.8.203"), v6("fe80::1:2:3:4")] } },
  { name: "eth + wifi same subnet", ifs: { lo: [v4("127.0.0.1", true)], eth0: [v4("10.0.0.111")], wlan0: [v4("10.0.0.112"), v4("10.0.0.113")] } },
  { name: "ipv6 only wifi", ifs: { lo: [v4("127.0.0.1", true)], wlan0: [v6("2001:db8::5")] } },
  { name: "node<18 numeric family", ifs: { eth0: [{ family: 4, address: "172.16.0.2", internal: false }] } },
  { name: "duplicate ip on two ifaces (first wins)", ifs: { wlan0: [v4("10.9.9.9")], wlan1: [v4("10.9.9.9")] } },
  { name: "empty", ifs: {} },
];
const IPS = [null, "", "127.0.0.1", "192.168.8.203", "192.168.225.37", "10.0.0.113", "10.0.0.111", "2001:db8::5", "172.16.0.2", "10.9.9.9", "203.0.113.7"];
const NAMES = ["lo", "eth0", "usb0", "wlan0", "wlan1", "nope"];
const cases = SNAPSHOTS.map((s) => {
  snap = s.ifs;
  return {
    name: s.name,
    interfaces: Object.entries(s.ifs).map(([n, addrs]) => [n, addrs.map((a) => ({ family: a.family === 4 ? "IPv4" : a.family, address: a.address, internal: a.internal }))]),
    ifaceForIp: IPS.map((ip) => [ip, ifaceForIp(ip)]),
    ipv4Of: NAMES.map((n) => [n, ipv4Of(n)]),
  };
});
console.log(JSON.stringify({ jsSourceFile: "src/hardware/network.ts", jsCommit: "89b5c64", generatedBy: "phase-4/evidence/wp5-tools/dump-iface-for-ip.mts → tools/replay/js/ via WP6 (Phase 4 WP5, CF22)", note: "a numeric family 4 (node<18) is normalised to \"IPv4\" in `interfaces` — the Rust snapshot type carries libuv's string family", cases }, null, 1));
