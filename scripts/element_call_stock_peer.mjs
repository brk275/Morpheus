#!/usr/bin/env node
// Observe the stock official Element Call image. No RTC refresh, membership
// injection, SDK patch, or getUserMedia replacement belongs in this harness.
// Cross-signing bootstraps only the disposable account's real SDK identity.
import { closeSync, constants, existsSync, fstatSync, linkSync, lstatSync, openSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

let stage = "configuration";
function mark(value) {
  stage = value;
  console.log(`MORPHEUS_ELEMENT_STAGE ${value}`);
}
function check(name) {
  console.log(`MORPHEUS_ELEMENT_CHECK ${name}=true`);
}
async function poll(callback, message, milliseconds = 90_000) {
  const end = Date.now() + milliseconds;
  while (Date.now() < end) {
    if (await callback()) return;
    await delay(200);
  }
  throw new Error(message);
}

// BEGIN pinned SDK44 public-wrapper observer. No peer methods are replaced.
function installStock44Observer() {
  const own = (object, key) => object && Object.getOwnPropertyDescriptor(object, key)?.value;
  const identitiesMatch = (member, owner) => {
    const values = member?.backendIdentities;
    return Array.isArray(values) && values.length > 0 && values.length <= 2 &&
      values.every((value) => typeof value === "string" && value.length > 0 && value.length <= 1_024) &&
      new Set(values).size === values.length && values.includes(owner.identity) &&
      member.userId === owner.user_id && member.deviceId === owner.device_id && member.memberId === owner.member_id;
  };
  window.__morpheusStock44Binding = async ({ owner, expectedMembers, kind, deadline }) => {
    if (!["audio", "video"].includes(kind) || !Number.isFinite(deadline) || deadline <= Date.now()) return null;
    if (kind === "video" && (typeof owner.track_sid !== "string" || !owner.track_sid.length || owner.track_sid.length > 1_024)) return null;
    const source = kind === "audio" ? "microphone" : "screenShare";
    const client = window.matrixclient;
    const session = window.rtcSession;
    const managed = client?.matrixRTC.roomSessions.get(owner.room_id);
    if (!managed || managed !== session || session.room?.roomId !== owner.room_id ||
        client.getDeviceId() === owner.device_id || session.memberships.length !== expectedMembers ||
        session.memberships.filter((member) => identitiesMatch(member, owner)).length !== 1) return null;
    const elements = [...document.querySelectorAll(kind === "audio" ? "audio" : 'video[data-source="screenShare"]')];
    if (elements.length > 32) throw new Error("Media element bound exceeded");
    const candidates = [];
    const inspect = (element) => {
      const keys = Object.keys(element).filter((key) => key.startsWith("__reactFiber$"));
      if (keys.length !== 1) return null;
      let fiber = own(element, keys[0]);
      const wrappers = new Set();
      const members = new Set();
      const audioOwners = new Set();
      for (let depth = 0; fiber && depth < 32; depth++) {
        const props = own(fiber, "memoizedProps");
        const wrapper = own(props, kind === "audio" ? "track" : "video");
        if (wrapper) wrappers.add(wrapper);
        const tracks = own(props, "tracks$");
        if (tracks) audioOwners.add(tracks);
        // InCallView's actual CallViewModel exposes the participation's same
        // remoteMembers$ Behavior. Do not read transport.resolved$ or tokens.
        const vm = own(props, "vm");
        const remote = own(vm, "remoteMembers$")?.value;
        if (remote !== undefined) {
          if (!Array.isArray(remote) || remote.length > 32) return null;
          for (const member of remote) if (member?.local === false && identitiesMatch(member, owner)) members.add(member);
        }
        fiber = own(fiber, "return");
      }
      if (members.size !== 1) return null;
      const member = [...members][0];
      const tracks = member.tracks$?.value;
      if (!Array.isArray(tracks) || tracks.length > 32 || (kind === "audio" && !audioOwners.has(member.tracks$))) return null;
      const matching = [...wrappers].filter((wrapper) => tracks.filter((track) => track === wrapper).length === 1 &&
        wrapper.kind === kind && wrapper.source === source && wrapper.encrypted === true &&
        wrapper.muted$?.value === false && typeof wrapper.id === "string" && wrapper.id.length > 0 &&
        wrapper.id.length <= 1_024 && (!owner.track_sid || wrapper.id === owner.track_sid) &&
        typeof wrapper.stats$ === "function");
      if (matching.length !== 1) return null;
      const track = matching[0];
      const stream = element.srcObject;
      if (!(stream instanceof MediaStream)) return null;
      const decoded = kind === "audio" ? stream.getAudioTracks() : stream.getVideoTracks();
      if (decoded.length !== 1) return null;
      const media = decoded[0];
      if (media.kind !== kind || media.readyState !== "live" || !media.enabled ||
          typeof media.id !== "string" || !media.id.length || media.id.length > 1_024 ||
          (kind === "audio" && (element.paused || element.muted || element.volume === 0))) return null;
      return { element, track, media, member };
    };
    for (const element of elements) {
      const candidate = inspect(element);
      if (candidate) candidates.push(candidate);
    }
    if (candidates.length !== 1) return null;
    const selected = candidates[0];
    // The pinned SDK reads the current publication's RemoteTrack stats. A
    // dictionary-only wrapper or an unrelated attached MediaStream cannot
    // establish this exact decoded trackIdentifier binding.
    const stats = await new Promise((resolve) => {
      let subscription;
      let finished = false;
      const finish = (value) => {
        if (finished) return;
        finished = true;
        clearTimeout(timer);
        subscription?.unsubscribe();
        resolve(value);
      };
      const timer = setTimeout(() => finish(null), Math.max(0, Math.min(1_000, deadline - Date.now())));
      try {
        subscription = selected.track.stats$(1_000).subscribe({ next: finish, error: () => finish(null), complete: () => finish(null) });
        if (finished) subscription.unsubscribe();
      } catch { finish(null); }
    });
    if (!stats || stats.type !== "inbound-rtp" || stats.kind !== kind ||
        typeof stats.trackIdentifier !== "string" || !stats.trackIdentifier.length ||
        stats.trackIdentifier.length > 1_024 || stats.trackIdentifier !== selected.media.id ||
        !Number.isFinite(stats.bytesReceived) || stats.bytesReceived <= 0 ||
        !Number.isFinite(stats.packetsReceived) || stats.packetsReceived <= 0 || Date.now() >= deadline) return null;
    const currentElements = [...document.querySelectorAll(kind === "audio" ? "audio" : 'video[data-source="screenShare"]')];
    if (currentElements.length > 32) return null;
    const currentMatches = currentElements.map(inspect).filter(Boolean);
    const current = currentMatches.length === 1 ? currentMatches[0] : null;
    if (!current || ["element", "track", "media", "member"].some((key) => current[key] !== selected[key]) ||
        window.matrixclient !== client || window.rtcSession !== session || client.matrixRTC.roomSessions.get(owner.room_id) !== session ||
        session.memberships.length !== expectedMembers ||
        session.memberships.filter((member) => identitiesMatch(member, owner)).length !== 1) return null;
    return selected;
  };
}
// END pinned SDK44 public-wrapper observer.

async function main() {
  // Even an inherited diagnostic flag invalidates stock-client evidence.
  if (process.env.MORPHEUS_ELEMENT_DIAG_REFRESH !== undefined)
    throw new Error("Diagnostic refresh is forbidden");
  const required = (key) => {
    const value = process.env[key];
    if (!value) throw new Error("Missing private fixture setting");
    return value;
  };
  const observerProfile = required("MORPHEUS_ELEMENT_OBSERVER_PROFILE");
  if (!["sdk42", "sdk44"].includes(observerProfile)) throw new Error("Unsupported observer profile");
  const root = required("MORPHEUS_ELEMENT_PEER_ROOT");
  const tool = required("MORPHEUS_ELEMENT_PLAYWRIGHT_ROOT");
  const base = new URL(required("MORPHEUS_ELEMENT_PEER_URL"));
  if (base.protocol !== "http:" || base.hostname !== "127.0.0.1" || !base.port ||
      base.username || base.password || base.pathname !== "/" || base.search || base.hash)
    throw new Error("Peer URL is not the owned loopback fixture");
  const fixture = JSON.parse(readFileSync(required("MORPHEUS_MATRIX_RTC_TEST_ACCOUNTS"), "utf8"));
  const account = fixture.accounts.find((entry) => entry.label === "a_second_device");
  const native = fixture.accounts.find((entry) => entry.label === "a_primary");
  const late = fixture.accounts.find((entry) => entry.label === "a_third_device");
  const roomId = fixture.rooms.encrypted_room;
  if (!account || !native || !late || !roomId) throw new Error("Incomplete disposable fixture");
  const { chromium } = await import(pathToFileURL(join(tool, "node_modules/playwright-core/index.mjs")));
  mark("browser_launch");
  const browser = await chromium.launch({
    headless: true,
    args: [
      "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
      `--use-file-for-fake-video-capture=${join(root, "camera.y4m")}`,
      `--use-file-for-fake-audio-capture=${join(root, "microphone.wav")}`,
      "--autoplay-policy=no-user-gesture-required",
    ],
  });
  let page;
  try {
    const context = await browser.newContext({ permissions: ["camera", "microphone"], viewport: { width: 1280, height: 850 } });
    page = await context.newPage();
    page.setDefaultTimeout(45_000);
    mark("browser_login");
    await page.goto(new URL("login", base).href);
    await page.getByTestId("login_username").fill(account.username);
    await page.getByTestId("login_password").fill(account.password);
    await page.getByTestId("login_login").click();
    await page.waitForURL((url) => url.origin === base.origin && url.pathname === "/");
    mark("browser_identity");
    await poll(() => page.evaluate(() => !!window.matrixclient?.getCrypto()), "SDK crypto did not initialize");
    await page.evaluate(async ({ username, password }) => {
      const client = window.matrixclient;
      const crypto = client?.getCrypto();
      if (!crypto) throw new Error("SDK crypto unavailable");
      await crypto.bootstrapCrossSigning({
        authUploadDeviceSigningKeys: async (request) => {
          try { await request(null); }
          catch (error) {
            if (!error?.data?.session) throw new Error("Unsupported cross-signing challenge");
            await request({ type: "m.login.password", identifier: { type: "m.id.user", user: username }, password, session: error.data.session });
          }
        },
      });
      await crypto.crossSignDevice(client.getDeviceId());
    }, { username: account.username, password: account.password });
    await poll(() => page.evaluate(async () => {
      const client = window.matrixclient;
      const status = await client?.getCrypto()?.getDeviceVerificationStatus(client.getUserId(), client.getDeviceId());
      return status?.crossSigningVerified === true;
    }), "Disposable browser device was not cross-signed");
    check("browser_identity");
    mark("browser_room_sync");
    await page.goto(new URL(`room/#/?roomId=${encodeURIComponent(roomId)}`, base).href);
    await page.getByTestId("lobby_joinCall").waitFor();
    await poll(() => page.evaluate(({ roomId, nativeId }) => {
      const client = window.matrixclient;
      const room = client?.getRoom(roomId);
      return ["PREPARED", "SYNCING"].includes(client?.getSyncState()) &&
        room?.getMyMembership() === "join" && room.getMember(nativeId)?.membership === "join";
    }, { roomId, nativeId: native.user_id }), "Browser initial room sync incomplete");
    // This is deliberately a pre-synced baseline. It does not prove a new
    // device can obtain keys for historical sticky memberships.
    writeFileSync(join(root, "browser-ready"), "ready\n", { mode: 0o600, flag: "wx" });
    check("browser_presynced");
    mark("native_join_wait");
    await poll(() => existsSync(join(root, "go")), "Native join did not become ready", 150_000);
    // These two new content-bearing receipts must become visible only after
    // their complete bounded JSON has been written and its exact file closed.
    // A hard link publishes without replacing an existing destination.
    const writeKeyReceipt = (name, index) => {
      if (!["group-observed.json", "new-key-observed.json"].includes(name) ||
          !Number.isInteger(index) || index < 0 || index > 255)
        throw new Error("Invalid key receipt");
      const bytes = Buffer.from(JSON.stringify({ key_index: index }), "utf8");
      if (bytes.length > 256) throw new Error("Key receipt exceeded bound");
      const ownerUid = BigInt(process.getuid());
      const parent = lstatSync(root, { bigint: true });
      const sameParent = () => {
        const current = lstatSync(root, { bigint: true });
        if (!current.isDirectory() || current.isSymbolicLink() || current.uid !== ownerUid ||
            (current.mode & 0o077n) !== 0n || current.dev !== parent.dev || current.ino !== parent.ino)
          throw new Error("Private receipt parent changed");
      };
      sameParent();
      const temporary = join(root, `${name}.pending`);
      const final = join(root, name);
      let descriptor;
      let created = false;
      let linked = false;
      let identity;
      let closeUnproved = false;
      const close = () => {
        const current = descriptor;
        descriptor = undefined; // A failed close is not retried on a possibly reused descriptor.
        try { closeSync(current); } catch { closeUnproved = true; throw new Error("Key receipt close unproved"); }
      };
      const exactFile = (path, links, complete = true) => {
        sameParent();
        const stat = lstatSync(path, { bigint: true });
        if (!identity || !stat.isFile() || stat.isSymbolicLink() || stat.uid !== ownerUid ||
            (stat.mode & 0o777n) !== 0o600n || stat.dev !== identity.dev || stat.ino !== identity.ino ||
            (complete ? stat.size !== BigInt(bytes.length) : stat.size > BigInt(bytes.length)) ||
            (links !== undefined && stat.nlink !== links))
          throw new Error("Owned key receipt identity changed");
      };
      try {
        descriptor = openSync(temporary,
          constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW | constants.O_NONBLOCK, 0o600);
        created = true;
        identity = fstatSync(descriptor, { bigint: true });
        if (!identity.isFile() || identity.uid !== ownerUid || (identity.mode & 0o777n) !== 0o600n ||
            identity.nlink !== 1n || identity.size !== 0n)
          throw new Error("Invalid exclusive key receipt");
        writeFileSync(descriptor, bytes);
        const written = fstatSync(descriptor, { bigint: true });
        if (written.dev !== identity.dev || written.ino !== identity.ino || written.size !== BigInt(bytes.length) || written.nlink !== 1n)
          throw new Error("Incomplete key receipt write");
        close();
        exactFile(temporary, 1n);
        linkSync(temporary, final);
        linked = true;
        exactFile(final, 2n);
        unlinkSync(temporary);
        created = false;
        exactFile(final, 1n);
      } catch {
        let cleaned = !closeUnproved;
        if (descriptor !== undefined) { try { close(); } catch { cleaned = false; } }
        // Remove only names whose actual inode still belongs to this attempt.
        // Never remove the existing target when no-clobber publication failed.
        for (const path of [...(linked ? [final] : []), ...(created ? [temporary] : [])]) {
          try { exactFile(path, undefined, false); unlinkSync(path); } catch { cleaned = false; }
        }
        throw new Error(cleaned ? "Key receipt publication failed" : "Key receipt cleanup unproved");
      }
    };
    const readOwner = (name, expectedUser, screen = false) => {
      const bytes = readFileSync(join(root, name));
      if (bytes.length > 8_192) throw new Error("Media owner exceeded bound");
      const value = JSON.parse(bytes.toString("utf8"));
      if (value.room_id !== roomId || value.user_id !== expectedUser ||
          ![value.user_id, value.device_id, value.member_id, value.identity].every(
            (entry) => typeof entry === "string" && entry.length > 0 && entry.length <= 1_024) ||
          (screen && (typeof value.track_sid !== "string" || !value.track_sid || value.track_sid.length > 1_024)))
        throw new Error("Media owner is incomplete");
      return value;
    };
    const owner = readOwner("native-audio-owner.json", native.user_id);
    // Observe the existing stock session before publishing the browser's call
    // membership. The callback never retains, exports or hashes its key bytes.
    await poll(() => page.evaluate((owner) => {
      const session = window.matrixclient?.matrixRTC.roomSessions.get(owner.room_id);
      if (!session) return false;
      if (window.__morpheusInteropKeys) return window.__morpheusInteropKeys.session === session;
      const state = { session, indices: new Set(), last: null, invalid: false };
      state.listener = (_key, index, member, identity) => {
        if (identity !== owner.identity || member?.userId !== owner.user_id ||
            member.deviceId !== owner.device_id || member.memberId !== owner.member_id) return;
        if (!Number.isInteger(index) || index < 0 || index > 255) { state.invalid = true; return; }
        state.indices.add(index);
        state.last = index;
      };
      session.on("encryption_key_changed", state.listener);
      window.__morpheusInteropKeys = state;
      return true;
    }, owner), "Existing stock session not ready for key observation");
    mark("browser_join");
    await page.getByTestId("lobby_joinCall").click();
    await page.getByTestId("incall_leave").waitFor({ timeout: 90_000 });
    mark("browser_roster");
    await poll(() => page.evaluate(({ roomId, nativeId, browserId }) => {
      const client = window.matrixclient;
      const session = window.rtcSession;
      const room = client?.getRoom(roomId);
      if (!room || !session) return false;
      // Read the pinned SDK's existing map. getRoomSession can construct a
      // missing session and would make this diagnostic alter RTC state.
      const managed = client.matrixRTC.roomSessions.get(roomId);
      const members = session?.memberships ?? [];
      return !!managed && session === managed && members.length === 2 && managed.memberships.length === 2 &&
        members.some((member) => member.userId === nativeId) &&
        members.some((member) => member.userId === browserId);
    }, { roomId, nativeId: native.user_id, browserId: account.user_id }), "Stock browser live roster did not converge");
    await poll(async () => await page.getByTestId("videoTile").count() === 2, "Stock browser participant tiles missing");
    check("browser_roster");
    mark("browser_receive_video");
    // Native video is white-left/black-right; Chromium's local fake camera is
    // red-left/green-right. The local preview cannot satisfy this assertion.
    let decodedSamples = 0;
    await poll(async () => {
      const decoded = await page.locator("video").evaluateAll((videos) => videos.some((video) => {
        if (video.readyState < 2 || video.videoWidth < 160 || video.videoHeight < 90) return false;
        const canvas = document.createElement("canvas");
        canvas.width = video.videoWidth;
        canvas.height = video.videoHeight;
        const ctx = canvas.getContext("2d");
        if (!ctx) return false;
        ctx.drawImage(video, 0, 0);
        const left = ctx.getImageData(Math.floor(canvas.width / 4), Math.floor(canvas.height / 2), 1, 1).data;
        const right = ctx.getImageData(Math.floor(canvas.width * 3 / 4), Math.floor(canvas.height / 2), 1, 1).data;
        return left[0] > 190 && left[1] > 190 && left[2] > 190 && right[0] < 55 && right[1] < 55 && right[2] < 55;
      }));
      decodedSamples = decoded ? decodedSamples + 1 : 0;
      return decodedSamples >= 3;
    }, "Browser did not decode native video");
    check("browser_video");
    if (observerProfile === "sdk44") await page.evaluate(installStock44Observer);
    mark("browser_receive_audio");
    async function observeAudio(owner, frequency, expectedMembers, original = false) {
      await page.evaluate(async ({ owner, frequency, expectedMembers, original, observerProfile }) => {
        // Read-only inspection of the pinned RC's React AudioTrack binding.
        // MatrixAudioRenderer -> AudioTrackWithAudioNodes -> AudioTrack supplies
        // trackRef. Never patch the stock peer, its SDK, or capture APIs. An
        // absent/changed binding fails closed instead of selecting any audio.
        const binding = () => {
          if (observerProfile === "sdk44") return window.__morpheusStock44Binding({ owner, expectedMembers, kind: "audio", deadline });
          const client = window.matrixclient;
          const session = window.rtcSession;
          const managed = client?.matrixRTC.roomSessions.get(owner.room_id);
          const members = session?.memberships ?? [];
          if (!managed || managed !== session || session.room?.roomId !== owner.room_id ||
              members.length !== expectedMembers || managed.memberships.length !== expectedMembers ||
              client.getDeviceId() === owner.device_id ||
              members.filter((member) => member.userId === owner.user_id &&
                member.deviceId === owner.device_id && member.memberId === owner.member_id &&
                member.rtcBackendIdentity === owner.identity).length !== 1) return null;
          const audios = [...document.querySelectorAll("audio")];
          if (audios.length > 32) throw new Error("Audio element bound exceeded");
          const matches = [];
          for (const element of audios) {
            const keys = Object.keys(element).filter((key) => key.startsWith("__reactFiber$"));
            if (keys.length !== 1) continue;
            let fiber = Object.getOwnPropertyDescriptor(element, keys[0])?.value;
            const refs = new Set();
            for (let depth = 0; fiber && depth < 32; depth++) {
              const props = Object.getOwnPropertyDescriptor(fiber, "memoizedProps")?.value;
              const ref = props && Object.getOwnPropertyDescriptor(props, "trackRef")?.value;
              if (ref) refs.add(ref);
              fiber = Object.getOwnPropertyDescriptor(fiber, "return")?.value;
            }
            for (const ref of refs) {
              const { participant, publication } = ref;
              const track = publication?.track;
              const media = track?.mediaStreamTrack;
              const stream = element.srcObject;
              if (participant?.isLocal !== false || participant.identity !== owner.identity ||
                  ref.source !== "microphone" || publication?.source !== "microphone" ||
                  publication.kind !== "audio" || publication.isLocal !== false ||
                  publication.isSubscribed !== true || !publication.isEncrypted ||
                  publication.isMuted || !publication.trackSid || track?.sid !== publication.trackSid ||
                  (owner.track_sid && publication.trackSid !== owner.track_sid) ||
                  track.kind !== "audio" || track.source !== "microphone" ||
                  participant.audioTrackPublications?.get(publication.trackSid) !== publication ||
                  participant.getTrackPublicationBySid(publication.trackSid) !== publication ||
                  !track.attachedElements.includes(element) || !(stream instanceof MediaStream) ||
                  stream.getAudioTracks().length !== 1 || stream.getAudioTracks()[0] !== media ||
                  media?.kind !== "audio" || media.readyState !== "live" || !media.enabled ||
                  element.paused || element.muted || element.volume === 0) continue;
              if (!matches.some((match) => match.element === element && match.track === track))
                matches.push({ element, track, media, participant, publication });
            }
          }
          return matches.length === 1 ? matches[0] : null;
        };
        const deadline = Date.now() + 45_000;
        let selected;
        while (!(selected = await binding())) {
          if (Date.now() >= deadline) throw new Error("Native remote microphone binding missing");
          await new Promise((resolve) => setTimeout(resolve, 200));
        }
        if (original) {
          const state = window.__morpheusInteropKeys;
          if (!state || state.invalid || state.session !== window.rtcSession)
            throw new Error("Original microphone key owner changed");
          if (!state.audio) state.audio = selected;
          if (["track", "media", "publication", "participant", "member"].some((key) => state.audio[key] !== selected[key]))
            throw new Error("Original microphone binding changed");
        }
        const context = new AudioContext();
        let clone;
        let source;
        let analyser;
        try {
          // This is a clone of the browser's decoded REMOTE track. No oscillator,
          // browser microphone, injected PCM, or network sender can feed it.
          clone = selected.media.clone();
          source = context.createMediaStreamSource(new MediaStream([clone]));
          analyser = context.createAnalyser();
          analyser.fftSize = 4_096;
          analyser.smoothingTimeConstant = 0;
          source.connect(analyser);
          let resumeDeadline;
          try {
            await Promise.race([
              context.resume(),
              new Promise((_, reject) => {
                resumeDeadline = setTimeout(() => reject(new Error("Audio context did not start")),
                  Math.max(0, deadline - Date.now()));
              }),
            ]);
          } finally {
            clearTimeout(resumeDeadline);
          }
          const samples = new Float32Array(analyser.fftSize);
          const spectrum = new Float32Array(analyser.frequencyBinCount);
          const binHz = context.sampleRate / analyser.fftSize;
          let consecutive = 0;
          while (Date.now() < deadline) {
            const current = await binding();
            if (!current || current.element !== selected.element || current.track !== selected.track ||
                current.media !== selected.media || current.participant !== selected.participant ||
                current.publication !== selected.publication || current.member !== selected.member || context.state !== "running")
              throw new Error("Native remote microphone ownership changed");
            analyser.getFloatTimeDomainData(samples);
            analyser.getFloatFrequencyData(spectrum);
            const rms = Math.sqrt(samples.reduce((sum, value) => sum + value * value, 0) / samples.length);
            let total = 0;
            let native = 0;
            let local = 0;
            let peak = 0;
            for (let index = 1; index < spectrum.length; index++) {
              const power = 10 ** (spectrum[index] / 10);
              const hz = index * binHz;
              total += power;
              if (Math.abs(hz - frequency) <= 35) native += power;
              if (Math.abs(hz - 440) <= 35) local += power;
              if (spectrum[index] > spectrum[peak]) peak = index;
            }
            const decoded = Number.isFinite(rms) && rms > 0.02 && total > 0 &&
              Math.abs(peak * binHz - frequency) <= 35 && native / total > 0.8 && native > local * 20;
            consecutive = decoded ? consecutive + 1 : 0;
            if (consecutive >= 3) return;
            await new Promise((resolve) => setTimeout(resolve, 200));
          }
          throw new Error("Browser did not decode the native microphone tone");
        } finally {
          source?.disconnect();
          analyser?.disconnect();
          clone?.stop();
          await context.close();
        }
      }, { owner, frequency, expectedMembers, original, observerProfile });
    }
    await observeAudio(owner, 997, 2, true);
    check("browser_audio");
    await poll(() => page.evaluate(() => {
      const state = window.__morpheusInteropKeys;
      if (!state || state.invalid || state.session !== window.rtcSession) throw new Error("Key owner changed");
      if (state.last === null) return false;
      state.baseline = state.last;
      return true;
    }), "Initial exact-sender media key not observed");
    writeFileSync(join(root, "browser-observed"), "observed\n", { mode: 0o600, flag: "wx" });
    mark("browser_group_roster");
    await poll(() => existsSync(join(root, "group-ready")), "Late peer did not become ready", 150_000);
    const groupOwner = readOwner("group-audio-owner.json", late.user_id);
    const screenOwner = readOwner("screen-owner.json", native.user_id, true);
    const microphoneSid = readFileSync(join(root, "native-microphone-sid"));
    if (!microphoneSid.length || microphoneSid.length > 1_024) throw new Error("Invalid native microphone SID");
    owner.track_sid = microphoneSid.toString("utf8");
    if (["room_id", "user_id", "device_id", "member_id", "identity"].some((key) => screenOwner[key] !== owner[key]))
      throw new Error("Screen is not owned by the original native sender");
    const roster = async (users) => await page.evaluate(({ roomId, users }) => {
      const session = window.rtcSession;
      const managed = window.matrixclient?.matrixRTC.roomSessions.get(roomId);
      if (!session || session !== managed) return false;
      const actual = session.memberships.map((member) => member.userId).sort();
      return actual.length === users.length && actual.every((user, index) => user === [...users].sort()[index]);
    }, { roomId, users });
    await poll(() => roster([native.user_id, account.user_id, late.user_id]), "Three stock memberships missing");
    check("browser_group_roster");
    mark("browser_group_audio");
    await observeAudio(groupOwner, 1700, 3);
    check("browser_group_audio");

    async function observeScreen(changed, expectedMembers) {
      await page.evaluate(async ({ owner, changed, expectedMembers, observerProfile }) => {
        const binding = () => {
          if (observerProfile === "sdk44") return window.__morpheusStock44Binding({ owner, expectedMembers, kind: "video", deadline });
          const session = window.rtcSession;
          const managed = window.matrixclient?.matrixRTC.roomSessions.get(owner.room_id);
          if (!session || session !== managed || session.memberships.length !== expectedMembers ||
              session.memberships.filter((member) => member.userId === owner.user_id &&
                member.deviceId === owner.device_id && member.memberId === owner.member_id &&
                member.rtcBackendIdentity === owner.identity).length !== 1) return null;
          const videos = [...document.querySelectorAll('video[data-lk-source="screen_share"]')];
          if (videos.length > 32) throw new Error("Screen element bound exceeded");
          const matches = [];
          for (const element of videos) {
            const keys = Object.keys(element).filter((key) => key.startsWith("__reactFiber$"));
            if (keys.length !== 1) continue;
            let fiber = Object.getOwnPropertyDescriptor(element, keys[0])?.value;
            const refs = new Set();
            for (let depth = 0; fiber && depth < 32; depth++) {
              const props = Object.getOwnPropertyDescriptor(fiber, "memoizedProps")?.value;
              const ref = props && Object.getOwnPropertyDescriptor(props, "trackRef")?.value;
              if (ref) refs.add(ref);
              fiber = Object.getOwnPropertyDescriptor(fiber, "return")?.value;
            }
            for (const ref of refs) {
              const { participant, publication } = ref;
              const track = publication?.track;
              const media = track?.mediaStreamTrack;
              const stream = element.srcObject;
              if (participant?.isLocal !== false || participant.identity !== owner.identity ||
                  ref.source !== "screen_share" || publication?.source !== "screen_share" ||
                  publication.kind !== "video" || publication.isLocal !== false || !publication.isEncrypted ||
                  publication.isSubscribed !== true || publication.isMuted || publication.trackSid !== owner.track_sid ||
                  track?.sid !== owner.track_sid || track.kind !== "video" || track.source !== "screen_share" ||
                  participant.videoTrackPublications?.get(owner.track_sid) !== publication ||
                  participant.getTrackPublicationBySid(owner.track_sid) !== publication ||
                  !track.attachedElements.includes(element) || !(stream instanceof MediaStream) ||
                  stream.getVideoTracks().length !== 1 || stream.getVideoTracks()[0] !== media ||
                  media?.kind !== "video" || media.readyState !== "live" || !media.enabled) continue;
              if (!matches.some((value) => value.element === element && value.track === track))
                matches.push({ element, track, media, publication, participant });
            }
          }
          return matches.length === 1 ? matches[0] : null;
        };
        const state = window.__morpheusInteropKeys;
        if (!state || state.invalid || state.session !== window.rtcSession) throw new Error("Screen key owner changed");
        const deadline = Date.now() + 45_000;
        let consecutive = 0;
        while (Date.now() < deadline) {
          const current = await binding();
          if (current) {
            if (!state.screen) state.screen = current;
            if (["track", "media", "publication", "participant", "member"].some((key) => state.screen[key] !== current[key]))
              throw new Error("Original screen binding changed");
            const video = current.element;
            if (video.readyState >= 2 && video.videoWidth >= 160 && video.videoHeight >= 90) {
              const canvas = document.createElement("canvas");
              canvas.width = video.videoWidth;
              canvas.height = video.videoHeight;
              const context = canvas.getContext("2d");
              if (!context) throw new Error("Screen pixel reader missing");
              context.drawImage(video, 0, 0);
              const top = context.getImageData(Math.floor(canvas.width / 2), Math.floor(canvas.height / 4), 1, 1).data;
              const bottom = context.getImageData(Math.floor(canvas.width / 2), Math.floor(canvas.height * 3 / 4), 1, 1).data;
              const white = (pixel) => pixel[0] > 190 && pixel[1] > 190 && pixel[2] > 190;
              const black = (pixel) => pixel[0] < 55 && pixel[1] < 55 && pixel[2] < 55;
              const decoded = changed ? black(top) && white(bottom) : white(top) && black(bottom);
              consecutive = decoded ? consecutive + 1 : 0;
              if (consecutive >= 3) return;
            } else consecutive = 0;
          } else consecutive = 0;
          await new Promise((resolve) => setTimeout(resolve, 200));
        }
        throw new Error("Exact encrypted screen picture did not decode");
      }, { owner: screenOwner, changed, expectedMembers, observerProfile });
    }
    mark("browser_receive_screen");
    await observeScreen(false, 3);
    check("browser_screen");
    await poll(() => page.evaluate(() => {
      const state = window.__morpheusInteropKeys;
      if (!state || state.invalid || state.session !== window.rtcSession) throw new Error("Key owner changed");
      if (state.last === null || state.last === state.baseline) return false;
      state.group = state.last;
      return true;
    }), "Late arrival did not deliver a new exact-sender key");
    const beforeLeave = await page.evaluate(() => window.__morpheusInteropKeys.group);
    writeKeyReceipt("group-observed.json", beforeLeave);
    mark("browser_group_departure");
    await poll(() => roster([native.user_id, account.user_id]), "Late peer did not depart stock roster");
    check("browser_group_departure");
    mark("browser_key_rotation");
    await poll(() => page.evaluate(() => {
      const state = window.__morpheusInteropKeys;
      if (!state || state.invalid || state.session !== window.rtcSession) throw new Error("Key owner changed");
      return state.last !== null && state.last !== state.group;
    }), "Departure did not deliver a new exact-sender key");
    const afterLeave = await page.evaluate(() => window.__morpheusInteropKeys.last);
    writeKeyReceipt("new-key-observed.json", afterLeave);
    await poll(() => existsSync(join(root, "screen-changed")), "Actual sender key activation did not finish");
    await observeScreen(true, 2);
    await observeAudio(owner, 1301, 2, true);
    check("browser_key_rotation");
    check("browser_post_rotation_screen");
    check("browser_post_rotation_audio");
    writeFileSync(join(root, "screen-change-observed"), "observed\n", { mode: 0o600, flag: "wx" });
    mark("native_media_wait");
    await poll(() => existsSync(join(root, "done")), "Native media checks did not complete", 150_000);
    mark("browser_leave");
    await page.getByTestId("incall_leave").click();
    await page.getByTestId("incall_leave").waitFor({ state: "hidden", timeout: 30_000 });
    check("browser_leave");
  } finally {
    try {
      if (page && !page.isClosed()) await page.evaluate(() => {
        const state = window.__morpheusInteropKeys;
        if (state) {
          state.session.off("encryption_key_changed", state.listener);
          delete window.__morpheusInteropKeys;
        }
      });
    } finally { await browser.close(); }
  }
}

try { await main(); }
catch {
  // Neither Playwright call logs nor exception payloads are public evidence.
  console.log(`MORPHEUS_ELEMENT_STAGE ${stage}`);
  console.log("MORPHEUS_ELEMENT_FAILURE browser_acceptance");
  process.exitCode = 1;
}
