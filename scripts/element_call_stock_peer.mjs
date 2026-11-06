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

async function main() {
  // Even an inherited diagnostic flag invalidates stock-client evidence.
  if (process.env.MORPHEUS_ELEMENT_DIAG_REFRESH !== undefined)
    throw new Error("Diagnostic refresh is forbidden");
  const required = (key) => {
    const value = process.env[key];
    if (!value) throw new Error("Missing private fixture setting");
    return value;
  };
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
    // Pinned 0.28.2 keeps LiveKit objects inside its SDK. Observe the public
    // wrapper, its exact remote member and the actual inbound decoded DOM track.
    // No stock functions, capture APIs, membership or keys are replaced.
    await page.evaluate(() => {
      const propsFor = (element) => {
        const keys = Object.keys(element).filter((key) => key.startsWith("__reactFiber$"));
        if (keys.length !== 1) return [];
        let fiber = Object.getOwnPropertyDescriptor(element, keys[0])?.value;
        const props = [];
        for (let depth = 0; fiber && depth < 32; depth++) {
          const value = Object.getOwnPropertyDescriptor(fiber, "memoizedProps")?.value;
          if (value && typeof value === "object") props.push(value);
          fiber = Object.getOwnPropertyDescriptor(fiber, "return")?.value;
        }
        return props;
      };
      const transportId = (raw) => raw && JSON.stringify(raw, Object.keys(raw).sort());
      const binding = async (owner, kind, source, expectedMembers, deadline) => {
        const client = window.matrixclient;
        const session = window.rtcSession;
        const managed = client?.matrixRTC.roomSessions.get(owner.room_id);
        const members = session?.memberships ?? [];
        if (!managed || managed !== session || session.room?.roomId !== owner.room_id ||
            members.length !== expectedMembers || managed.memberships.length !== expectedMembers ||
            client.getDeviceId() === owner.device_id) return null;
        const owners = members.filter((member) => member.userId === owner.user_id &&
          member.deviceId === owner.device_id && member.memberId === owner.member_id &&
          member.backendIdentities?.includes(owner.identity));
        if (owners.length !== 1) return null;
        const membership = owners[0];
        const audios = [...document.querySelectorAll("audio")];
        const elements = kind === "audio" ? audios : [...document.querySelectorAll("video")];
        if (audios.length > 32 || elements.length > 32) throw new Error("Media element bound exceeded");
        // MemberAudioRenderer owns this array, and MemberAudio receives the
        // same tracks$ object. The SDK filters remote members on their transport.
        const remoteMembers = new Set();
        for (const audio of audios) for (const props of propsFor(audio)) {
          if (!Array.isArray(props.members)) continue;
          if (props.members.length !== expectedMembers - 1) continue;
          for (const member of props.members) {
            if (member?.local === false && member.userId === owner.user_id &&
                member.deviceId === owner.device_id && member.memberId === owner.member_id &&
                member.backendIdentities?.includes(owner.identity)) remoteMembers.add(member);
          }
        }
        if (remoteMembers.size !== 1) return null;
        const participant = [...remoteMembers][0];
        const transport = participant.transport$?.value;
        if (transport?.type !== "livekit" || transport.id !== transportId(transport.raw) ||
            !membership.transports.some((raw) => transportId(raw) === transport.id)) return null;
        const tracks = participant.tracks$?.value;
        if (!Array.isArray(tracks) || tracks.length > 32) return null;
        const matches = [];
        for (const element of elements) {
          const props = propsFor(element);
          const wrappers = new Set(props.map((value) => kind === "audio" ? value.track : value.video).filter(Boolean));
          for (const track of wrappers) {
            if (Date.now() >= deadline) return null;
            const stream = element.srcObject;
            const mediaTracks = stream instanceof MediaStream ?
              (kind === "audio" ? stream.getAudioTracks() : stream.getVideoTracks()) : [];
            const media = mediaTracks[0];
            if (tracks.filter((value) => value === track).length !== 1 ||
                track.kind !== kind || track.source !== source || track.encrypted !== true ||
                typeof track.id !== "string" || !track.id || track.id.length > 1_024 ||
                (owner.track_sid && track.id !== owner.track_sid) ||
                track.muted$?.value !== false || typeof track.stats$ !== "function" ||
                mediaTracks.length !== 1 || media?.kind !== kind || media.readyState !== "live" || !media.enabled ||
                (kind === "audio" && (element.paused || element.muted || element.volume === 0)) ||
                (kind === "audio" && !props.some((value) => value.tracks$ === participant.tracks$))) continue;
            let subscription;
            let timer;
            let settled = false;
            const stats = await new Promise((resolve) => {
              const finish = (value) => { if (!settled) { settled = true; resolve(value); } };
              timer = setTimeout(() => finish(null), Math.max(0, Math.min(1_000, deadline - Date.now())));
              try {
                subscription = track.stats$(200).subscribe({
                  next: (value) => finish(value), error: () => finish(null), complete: () => finish(null),
                });
              } catch { finish(null); }
            }).finally(() => { clearTimeout(timer); subscription?.unsubscribe(); });
            // stats$ polls this exact wrapper's publication. A subscribed remote
            // RTP receiver's trackIdentifier binds that publication to srcObject.
            if (Date.now() >= deadline || !stats || stats.type !== "inbound-rtp" || stats.kind !== kind ||
                stats.trackIdentifier !== media.id || !(stats.bytesReceived > 0) ||
                !(stats.packetsReceived > 0) ||
                (kind === "video" && !(stats.framesDecoded > 0))) continue;
            // Async stats must not validate a DOM/member binding replaced while
            // the receiver was being polled.
            const currentMedia = kind === "audio" ? stream.getAudioTracks() : stream.getVideoTracks();
            if (window.rtcSession !== session || client.matrixRTC.roomSessions.get(owner.room_id) !== managed ||
                session.memberships.length !== expectedMembers || !session.memberships.includes(membership) ||
                !membership.backendIdentities?.includes(owner.identity) ||
                element.srcObject !== stream || currentMedia.length !== 1 || currentMedia[0] !== media ||
                media.readyState !== "live" || !media.enabled || track.muted$?.value !== false ||
                !participant.tracks$?.value?.includes(track) || participant.transport$?.value !== transport ||
                !membership.transports.some((raw) => transportId(raw) === transport.id))
              continue;
            if (!matches.some((value) => value.element === element && value.track === track))
              matches.push({ element, track, media, participant, publication: track.id, transport: transport.id });
          }
        }
        return matches.length === 1 ? matches[0] : null;
      };
      window.__morpheusStockBinding = binding;
    });
    mark("browser_receive_audio");
    async function observeAudio(owner, frequency, expectedMembers, original = false) {
      await page.evaluate(async ({ owner, frequency, expectedMembers, original }) => {
        const binding = () => window.__morpheusStockBinding(owner, "audio", "microphone", expectedMembers, deadline);
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
          if (["track", "media", "publication", "participant", "transport"].some((key) => state.audio[key] !== selected[key]))
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
                current.publication !== selected.publication || current.transport !== selected.transport || context.state !== "running")
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
      }, { owner, frequency, expectedMembers, original });
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
      await page.evaluate(async ({ owner, changed, expectedMembers }) => {
        const binding = () => window.__morpheusStockBinding(owner, "video", "screenShare", expectedMembers, deadline);
        const state = window.__morpheusInteropKeys;
        if (!state || state.invalid || state.session !== window.rtcSession) throw new Error("Screen key owner changed");
        const deadline = Date.now() + 45_000;
        let consecutive = 0;
        while (Date.now() < deadline) {
          const current = await binding();
          if (current) {
            if (!state.screen) state.screen = current;
            if (["track", "media", "publication", "participant", "transport"].some((key) => state.screen[key] !== current[key]))
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
      }, { owner: screenOwner, changed, expectedMembers });
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
        delete window.__morpheusStockBinding;
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
