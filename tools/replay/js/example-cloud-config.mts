// Example white-box driver (tools/replay/js/run-tsx.sh example-cloud-config.mts):
// prints what the TS cloud-config parser makes of the synth calibration
// tape's retained documents. Drivers import from ../src (the scratch copy).
import { parseCloudMessage } from "../src/agent/cloud-config.js";

const docs = [
  { v: 1, rev: 1, gnssAntBias: true, timeSync: { offlineSources: true, stepThresholdSecs: 10, maxStepsPerBoot: 3 } },
  { v: 1, rev: 2, imuMount: { enabled: true, rollDeg: 90, pitchDeg: 0, yawDeg: 0 } },
];
for (const d of docs) {
  console.log(JSON.stringify(parseCloudMessage(Buffer.from(JSON.stringify(d)))));
}
