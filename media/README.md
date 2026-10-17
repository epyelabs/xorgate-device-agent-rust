# media/ — KVS WebRTC runtime

`xorgate-kvs-webrtc` is the compiled AWS KVS WebRTC **master** runtime (ARM aarch64),
built from [`xorgate/device/xorgate-kvs-webrtc`](../../xorgate-kvs-webrtc). The device
agent's media collector ([`src/agent/collectors/media.ts`](../src/agent/collectors/media.ts))
supervises one instance of it per KVS signaling channel so the device streams live video
to the frontend's Streams-tab viewer.

It is invoked as `xorgate-kvs-webrtc <channelName> <mediaType> <source>` (e.g.
`<deviceId>-cam0 video-only testsrc`) with the device's IoT credentials passed via the
environment (see the collector). On the device it lands at `/opt/xorgate/media/`.

**Build requirement:** the binary MUST be built from a `xorgate-kvs-webrtc` revision where
credential mode is selected at **runtime** (the "flexible switch on static vs IoT core mode"
commit). An older build that gates the IoT path behind `#ifdef IOT_CORE_ENABLE_CREDENTIALS`
(with CMake not defining it) compiles to static-keys-only and fails at startup with
`AWS_ACCESS_KEY_ID must be set`, even when the IoT env vars are set. Verify with:
`strings xorgate-kvs-webrtc | grep AWS_IOT_CORE_CREDENTIAL_ENDPOINT` (must be present).

## `lib/` — shared libraries (supplied separately)

The binary is **dynamically linked**. Per `ldd` on the reference Pi, these are the libs it
resolves out of the SDK build tree (not apt packages), so they must live here in `lib/` and
travel with the binary. The runtime is launched with `LD_LIBRARY_PATH` pointed here
(`/opt/xorgate/media/lib` on a device):

| File | Built into | Note |
|------|------------|------|
| `libkvsWebrtcClient.so` | `…/build/` | srtp2 / usrsctp / kvspic\* are statically linked inside |
| `libkvsWebrtcSignalingClient.so` | `…/build/` | |
| `libkvsCommonLws.so.1` | `…/open-source/lib/` | |
| `libwebsockets.so.19` | `…/open-source/lib/` | SDK-vendored |
| `libssl.so.1.1` | `…/open-source/lib/` | **OpenSSL 1.1**, vendored (current Pi OS ships OpenSSL 3) |
| `libcrypto.so.1.1` | `…/open-source/lib/` | **OpenSSL 1.1**, vendored |

GStreamer + glib (and their deps) come from **apt** and are installed by the device install
script. OpenSSL and libwebsockets are **not** taken from apt — the SDK vendors its own
OpenSSL 1.1, which differs from the system OpenSSL 3, so those ship here.

Pull them off the build device with [`fetch-kvs-libs.sh`](fetch-kvs-libs.sh) (asks the Pi's
`ldd` what the binary links and scp's exactly those into `lib/`):

```sh
media/fetch-kvs-libs.sh pi@10.0.0.111
```

Until the libs are present, the runtime exits immediately on start; the media collector
detects the repeated quick failures, logs once, and parks the channel (it never tight-loops).
Drop the libs in, then `xorgate set media on` and restart the agent.

## `cert.pem` — CA bundle (required)

`cert.pem` is the CA bundle the runtime validates TLS against, for BOTH the IoT
credential-provider endpoint and the KVS control/signaling plane (`AWS_KVS_CACERT_PATH`,
which the agent defaults to `<binary-dir>/cert.pem`). It is the SDK's bundled
**Starfield Services Root Certificate Authority - G2**.

This is a real gotcha: the device's `root-ca.pem` (Amazon Root CA 1) validates MQTT but the
KVS endpoints chain to the Starfield root, so reusing the device root CA fails with
`unable to get local issuer certificate (X509_V_ERR=20)`. The Debian system bundle
(`/etc/ssl/certs/ca-certificates.crt`) was also observed to fail; ship and use this
`cert.pem`. It comes from `xorgate-kvs-webrtc/third_party/.../certs/cert.pem`.
