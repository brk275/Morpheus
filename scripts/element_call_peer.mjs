#!/usr/bin/env node
// Opt-in browser half of crates/morpheus-rtc/tests/element_call_peer.rs.
// Uses an external Element Call checkout and synthetic browser media only.
// Fixture passwords and Matrix tokens never enter command arguments or logs.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const checkout = process.env.MORPHEUS_ELEMENT_CALL_CHECKOUT;
const fixturePath = process.env.MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS;
const goFile = process.env.MORPHEUS_ELEMENT_PEER_GO_FILE;
const doneFile = process.env.MORPHEUS_ELEMENT_PEER_DONE_FILE;
if (!checkout || !fixturePath || !goFile || !doneFile) {
  throw new Error(
    "Set MORPHEUS_ELEMENT_CALL_CHECKOUT, MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS, " +
      "MORPHEUS_ELEMENT_PEER_GO_FILE, and MORPHEUS_ELEMENT_PEER_DONE_FILE",
  );
}
const { chromium } = await import(
  pathToFileURL(join(checkout, "node_modules/@playwright/test/index.mjs"))
);
const fixture = JSON.parse(readFileSync(fixturePath, "utf8"));
const account = fixture.accounts.find(
  (entry) => entry.label === "a_second_device",
);
if (!account || !fixture.rooms.encrypted_room) {
  throw new Error("Disposable browser account or encrypted room is missing");
}

async function waitForFile(path, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(path)) return;
    await delay(250);
  }
  throw new Error(`Timed out waiting for local coordination file: ${path}`);
}

const browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
const context = await browser.newContext({
  ignoreHTTPSErrors: true, // The upstream dev server uses a test certificate.
  permissions: ["camera", "microphone"],
  viewport: { width: 1280, height: 850 },
});
const page = await context.newPage();
const diagnostics = {
  missingKeyMessages: 0,
  noKeyTargetsMessages: 0,
  keyTransportErrors: 0,
  toDeviceRequests: 0,
  toDeviceFailures: 0,
  tokenResponses: {},
  stickyResponses: {},
  membershipWarnings: 0,
  syncResponses: 0,
  syncStickyFields: {},
};
page.on("console", (message) => {
  const value = message.text();
  if (value.includes("MissingKey")) diagnostics.missingKeyMessages++;
  if (value.includes("No targets found for sending key")) diagnostics.noKeyTargetsMessages++;
  if (value.includes("ToDeviceKeyTransport") && message.type() === "error")
    diagnostics.keyTransportErrors++;
  if (value.includes("Ignoring membership") || value.includes("Couldn't construct call membership"))
    diagnostics.membershipWarnings++;
});
page.on("request", (request) => {
  if (new URL(request.url()).pathname.includes("/sendToDevice/"))
    diagnostics.toDeviceRequests++;
});
page.on("response", (response) => {
  const path = new URL(response.url()).pathname;
  if (path.includes("/sendToDevice/") && response.status() >= 400)
    diagnostics.toDeviceFailures++;
  if (path.endsWith("/get_token"))
    diagnostics.tokenResponses[response.status()] =
      (diagnostics.tokenResponses[response.status()] ?? 0) + 1;
  if (path.includes("/sticky/") || path.includes("/unstable/org.matrix.msc4354"))
    diagnostics.stickyResponses[response.status()] =
      (diagnostics.stickyResponses[response.status()] ?? 0) + 1;
  if (path.endsWith("/sync") && response.status() === 200) {
    diagnostics.syncResponses++;
    void response.json().then((body) => {
      const joined = body.rooms?.join?.[fixture.rooms.encrypted_room];
      for (const key of Object.keys(joined ?? {}).filter((key) => key.includes("sticky"))) {
        diagnostics.syncStickyFields[key] =
          (diagnostics.syncStickyFields[key] ?? 0) + 1;
      }
    }).catch(() => {});
  }
});

// This host's Chrome and Playwright Chromium enumerate fake A/V devices but
// never resolve getUserMedia, even on a blank localhost page with permission
// granted. Canvas/Web Audio tracks still pass through Element Call's real
// MatrixRTC encryption and LiveKit stack. The patch is for the local test only.
await page.addInitScript(() => {
  const devices = navigator.mediaDevices;
  if (!devices) return;
  window.__morpheusSyntheticSources = [];
  devices.getUserMedia = async (constraints = {}) => {
    const tracks = [];
    if (constraints.video) {
      const canvas = document.createElement("canvas");
      canvas.width = 640;
      canvas.height = 360;
      const graphics = canvas.getContext("2d");
      let offset = 0;
      const draw = () => {
        graphics.fillStyle = "#db343a";
        graphics.fillRect(0, 0, 320, 360);
        graphics.fillStyle = "#21b66c";
        graphics.fillRect(320, 0, 320, 360);
        graphics.fillStyle = "#ffffff";
        graphics.fillRect(offset % 640, 0, 12, 360);
        offset += 8;
      };
      draw();
      const interval = setInterval(draw, 66);
      const stream = canvas.captureStream(15);
      tracks.push(...stream.getVideoTracks());
      window.__morpheusSyntheticSources.push({ canvas, stream, interval });
    }
    if (constraints.audio) {
      const context = new AudioContext();
      const oscillator = context.createOscillator();
      const gain = context.createGain();
      const destination = context.createMediaStreamDestination();
      oscillator.frequency.value = 440;
      gain.gain.value = 0.08;
      oscillator.connect(gain);
      gain.connect(destination);
      oscillator.start();
      await context.resume();
      tracks.push(...destination.stream.getAudioTracks());
      window.__morpheusSyntheticSources.push({ context, oscillator, destination });
    }
    return new MediaStream(tracks);
  };
});

try {
  await page.goto("https://127.0.0.1:5173/login", {
    waitUntil: "domcontentloaded",
  });
  await page.getByTestId("login_username").fill(account.username);
  await page.getByTestId("login_password").fill(account.password);
  await page.getByTestId("login_login").click();
  await page.waitForURL("https://127.0.0.1:5173/", { timeout: 30_000 });
  console.log("Element Call disposable browser login succeeded.");

  // The native RTC library accepts keys from a cross-signed sender device.
  // Element Call's plain password login does not bootstrap the disposable
  // account, so sign this new browser device before joining the call.
  const identity = await page.evaluate(async ({ username, password }) => {
    const client = window.matrixclient;
    const crypto = client?.getCrypto();
    if (!client || !crypto) throw new Error("Browser Matrix crypto is unavailable");
    await crypto.bootstrapCrossSigning({
      authUploadDeviceSigningKeys: async (makeRequest) => {
        try {
          return await makeRequest(null);
        } catch (error) {
          return await makeRequest({
            type: "m.login.password",
            identifier: { type: "m.id.user", user: username },
            password,
            session: error?.data?.session,
          });
        }
      },
    });
    await crypto.crossSignDevice(client.getDeviceId());
    let device;
    for (let attempt = 0; attempt < 30; attempt++) {
      device = await crypto.getDeviceVerificationStatus(
        client.getUserId(), client.getDeviceId(),
      );
      if (device?.crossSigningVerified === true) break;
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    return {
      crossSigningReady: await crypto.isCrossSigningReady(),
      ownDeviceCrossSigned: device?.crossSigningVerified === true,
    };
  }, { username: account.username, password: account.password });
  console.log("Element Call cross-signing readiness:", JSON.stringify(identity));
  if (!identity.crossSigningReady || !identity.ownDeviceCrossSigned)
    throw new Error("Disposable Element Call device was not cross-signed");
  console.log("Element Call disposable browser device is cross-signed.");

  const roomId = encodeURIComponent(fixture.rooms.encrypted_room);
  await page.goto(`https://127.0.0.1:5173/room/#/?roomId=${roomId}`, {
    waitUntil: "domcontentloaded",
  });
  await page.getByTestId("lobby_joinCall").waitFor({ timeout: 30_000 });
  await waitForFile(goFile, 120_000);
  await page.getByTestId("lobby_joinCall").click({ noWaitAfter: true });
  await page.getByTestId("incall_leave").waitFor({ timeout: 30_000 });
  console.log("Element Call entered Morpheus's encrypted room call.");

  await delay(8_000);
  const alice = fixture.accounts.find((entry) => entry.label === "a_primary");
  const status = await page.evaluate((aliceId) => {
    const client = window.matrixclient;
    const room = client?.getRoom(window.rtcSession?.room?.roomId);
    const activeSession = room ? client.matrixRTC.getRoomSession(room) : null;
    const memberships = activeSession?.memberships ?? [];
    const sticky = room ? [...room._unstable_getStickyEvents()] : [];
    return {
      route: window.location.pathname,
      callViewSessionPresent: !!window.rtcSession,
      memberships: memberships.length,
      seesNativePeer: memberships.some((member) => member.userId === aliceId),
      slotStatus: room ? client.matrixRTC.getRtcSlot(room)?.status ?? null : null,
      sticky: sticky.map((event) => ({
        type: event.getType(),
        slot: event.getContent().slot_id ?? null,
        isNativePeer: event.getSender() === aliceId,
        hasMember: !!event.getContent().member,
        hasTransport: !!event.getContent().transports,
        hasLegacyTransport: !!event.getContent().rtc_transports,
        isLeave: Object.keys(event.getContent()).every((key) => key === "msc4354_sticky_key"),
      })),
      sources: (window.__morpheusSyntheticSources ?? []).map((source) => ({
        audioContextState: source.context?.state ?? null,
        tracks: source.stream?.getTracks().map((track) => ({
          kind: track.kind,
          enabled: track.enabled,
          muted: track.muted,
          readyState: track.readyState,
        })) ?? source.destination?.stream.getTracks().map((track) => ({
          kind: track.kind,
          enabled: track.enabled,
          muted: track.muted,
          readyState: track.readyState,
        })) ?? [],
      })),
    };
  }, alice?.user_id);
  console.log("Element Call sanitized media/session status:", JSON.stringify(status));
  console.log("Element Call sanitized key transport counts:", JSON.stringify(diagnostics));

  await waitForFile(doneFile, 120_000);
  await page.getByTestId("incall_leave").click({ noWaitAfter: true });
  console.log("Element Call sent leave from the encrypted room call.");
  await delay(2_000);
} finally {
  await browser.close();
}
