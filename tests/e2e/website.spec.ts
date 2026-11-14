import { expect, test, type BrowserContext, type Page } from '@playwright/test';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { randomBytes, randomUUID } from 'node:crypto';
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const title = 'E2E Test Movie';
const password = randomBytes(24).toString('base64url');
let root: string | undefined;
let artifactsRoot: string | undefined;
let server: ChildProcess | undefined;
let worker: ChildProcess | undefined;
let baseURL: string;
let movieId: string;
let serverLog = '';
let retainState = false;
let incomingFile: string;
let dvdIncomingFile: string;
const sessionCookies = new Map<string, Awaited<ReturnType<BrowserContext['cookies']>>>();

function command(program: string, args: string[], input?: string, cwd = repo, extraEnv: NodeJS.ProcessEnv = {}): string {
  const result = spawnSync(program, args, {
    cwd,
    input,
    env: { ...process.env, ...extraEnv },
    encoding: 'utf8',
    timeout: 180_000,
    maxBuffer: 4 * 1024 * 1024,
  });
  if (result.error || result.status !== 0) {
    throw new Error(`${program} failed: ${result.error ?? result.stderr}`);
  }
  return result.stdout.trim();
}

function fixture(args: string[]) {
  const preview = spawnSync('safe-exec', [...args, '--preview'], {
    cwd: repo, encoding: 'utf8', timeout: 10_000, maxBuffer: 1024 * 1024,
  });
  if (preview.error || (preview.status !== 0 && preview.status !== 7)) {
    throw new Error(`Fixture preview failed: ${preview.error ?? preview.stderr}`);
  }
  command('safe-exec', args);
}

async function stopProcess(child?: ChildProcess) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise<void>((done) => {
    const timer = setTimeout(() => child.kill('SIGKILL'), 5_000);
    child.once('exit', () => {
      clearTimeout(timer);
      done();
    });
    child.kill('SIGTERM');
  });
}

test.beforeAll(async ({}, testInfo) => {
  test.setTimeout(240_000);
  const requestedArtifacts = process.env.WETUBE_E2E_ARTIFACTS_DIR;
  if (!requestedArtifacts) {
    throw new Error('Set WETUBE_E2E_ARTIFACTS_DIR to an existing directory under /mnt/c/TempPrivate; Linux ISO staging is prohibited.');
  }
  if (lstatSync(requestedArtifacts).isSymbolicLink()) {
    throw new Error('WETUBE_E2E_ARTIFACTS_DIR must not be a symbolic link.');
  }
  const allowedArtifacts = realpathSync('/mnt/c/TempPrivate');
  const artifactsBase = realpathSync(requestedArtifacts);
  const location = relative(allowedArtifacts, artifactsBase);
  if (location === '..' || location.startsWith(`..${sep}`) || isAbsolute(location)) {
    throw new Error('WETUBE_E2E_ARTIFACTS_DIR must resolve under /mnt/c/TempPrivate; Linux ISO staging is prohibited.');
  }
  retainState = testInfo.config.metadata.retainState === true;
  root = mkdtempSync(join(artifactsBase, 'wetube-browser-e2e-'));
  artifactsRoot = mkdtempSync(join(artifactsBase, 'wetube-media-e2e-'));
  const staticDir = join(artifactsRoot, 'frontend');
  command('cargo', ['build', '--locked', '--package', 'wetube-server', '--package', 'wetube-worker']);
  command('rustup', ['run', 'stable', '/home/henry/.cargo/bin/trunk', 'build', '--release', '--locked', '--dist', staticDir], undefined, join(repo, 'crates/web'));
  if (retainState) {
    const credentials = join(root, 'login.txt');
    writeFileSync(credentials, `Username: e2e-admin\nPassword: ${password}\nViewer: e2e-viewer (same test password)\n`, {
      flag: 'wx', mode: 0o600,
    });
    console.log(`Retained preview directory: ${root}`);
    console.log(`Retained media directory: ${artifactsRoot}`);
    console.log(`Local test credentials: ${credentials}`);
  }
  const state = join(root, 'state');
  const media = join(artifactsRoot, 'media');
  const incoming = join(artifactsRoot, 'incoming');
  const scratch = join(artifactsRoot, 'scratch');
  mkdirSync(incoming, { mode: 0o700 });
  mkdirSync(scratch, { mode: 0o700 });
  incomingFile = join(incoming, 'browser-invalid.iso');
  const attempt = randomUUID();
  const attemptDir = join(artifactsRoot, attempt);
  mkdirSync(media, { mode: 0o700 });
  mkdirSync(attemptDir, { mode: 0o700 });
  const movie = join(attemptDir, 'movie.mp4');
  const report = join(attemptDir, 'report.json');
  command('ffmpeg', [
    '-hide_banner', '-nostdin', '-v', 'error',
    '-f', 'lavfi', '-i', 'testsrc2=size=160x90:rate=24',
    '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000',
    '-t', '8', '-threads', '2', '-c:v', 'libx264', '-preset', 'ultrafast',
    '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-ac', '2', '-movflags', '+faststart', movie,
  ]);
  // Stream the payload hash rather than loading it into a size-limited Rhai blob.
  const digest = command('sha256sum', [movie]).split(/\s+/, 1)[0];
  fixture([
    join(repo, 'tests/e2e/fixture-report.rhai'), report, incomingFile, '--arg', `attempt=${attempt}`,
  ]);
  const dvdSource = join(artifactsRoot, 'dvd-source');
  const dvdDirectory = join(artifactsRoot, 'authored-dvd');
  mkdirSync(dvdSource, { mode: 0o700 });
  const longVob = join(dvdSource, 'long.vob');
  const shortVob = join(dvdSource, 'short.vob');
  for (const [vob, seconds] of [[longVob, '20'], [shortVob, '3']]) {
    command('ffmpeg', [
      '-hide_banner', '-nostdin', '-v', 'error',
      '-f', 'lavfi', '-i', 'testsrc2=size=720x480:rate=30000/1001',
      '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000',
      '-t', seconds, '-target', 'ntsc-dvd', '-aspect', '4:3', '-threads', '2',
      '-c:a', 'ac3', '-b:a', '192k', '-ac', '2', vob,
    ]);
  }
  const dvdXml = join(dvdSource, 'dvd.xml');
  fixture([
    join(repo, 'tests/e2e/fixture-dvd.rhai'), dvdXml,
    '--arg', `long_file=${longVob}`, '--arg', `short_file=${shortVob}`,
  ]);
  command('dvdauthor', ['-o', dvdDirectory, '-x', dvdXml], undefined, repo, { VIDEO_FORMAT: 'NTSC' });
  dvdIncomingFile = join(incoming, 'browser-valid.iso');
  command('genisoimage', ['-quiet', '-dvd-video', '-o', dvdIncomingFile, dvdDirectory]);
  const binary = join(repo, 'target/debug/wetube-server');
  command(binary, ['--state-dir', state, 'init']);
  command(binary, ['--state-dir', state, 'add-user', '--username', 'e2e-admin', '--admin', '--password-stdin'], `${password}\n`);
  command(binary, ['--state-dir', state, 'add-user', '--username', 'e2e-other-admin', '--admin', '--password-stdin'], `${password}\n`);
  command(binary, ['--state-dir', state, 'add-user', '--username', 'e2e-viewer', '--password-stdin'], `${password}\n`);
  const admitted = JSON.parse(command(binary, [
    '--state-dir', state, 'admit-preview', '--report', report,
    '--media-dir', media, '--title', title, '--movie-sha256', digest,
    '--description', 'Synthetic browser test fixture; not a real DVD import.',
  ]));
  movieId = admitted.movie_id;
  const port = await new Promise<number>((done, fail) => {
    const socket = createServer();
    socket.once('error', fail);
    socket.listen(0, '127.0.0.1', () => {
      const address = socket.address();
      if (!address || typeof address === 'string') {
        socket.close();
        fail(new Error('Could not allocate a local test port'));
        return;
      }
      socket.close(error => error ? fail(error) : done(address.port));
    });
  });
  baseURL = `http://127.0.0.1:${port}`;
  server = spawn(binary, [
    '--state-dir', state, 'serve', '--bind', `127.0.0.1:${port}`,
    '--origin', baseURL,
    '--static-dir', staticDir, '--media-dir', media, '--incoming-dir', incoming,
    '--scratch-dir', scratch,
  ], { cwd: repo, stdio: ['ignore', 'pipe', 'pipe'] });
  server.stdout?.on('data', chunk => { serverLog = (serverLog + chunk).slice(-16_384); });
  server.stderr?.on('data', chunk => { serverLog = (serverLog + chunk).slice(-16_384); });
  worker = spawn(join(repo, 'target/debug/wetube-worker'), [
    '--state-dir', state, 'run', '--incoming-dir', incoming, '--scratch-dir', scratch,
    '--media-dir', media,
  ], { cwd: repo, stdio: ['ignore', 'ignore', 'pipe'] });
  worker.stderr?.on('data', chunk => { serverLog = (serverLog + chunk).slice(-16_384); });
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    if (server.exitCode !== null || server.signalCode !== null) throw new Error(`Server exited: ${serverLog}`);
    if (worker.exitCode !== null || worker.signalCode !== null) throw new Error(`Worker exited: ${serverLog}`);
    try {
      const response = await fetch(`${baseURL}/health/ready`);
      if (response.ok) return;
    } catch {}
    await new Promise(done => setTimeout(done, 100));
  }
  throw new Error(`Server did not become ready: ${serverLog}`);
});

test.afterAll(async () => {
  await stopProcess(worker);
  await stopProcess(server);
  if (artifactsRoot && !retainState) rmSync(artifactsRoot, { recursive: true, force: true });
  if (root && !retainState) rmSync(root, { recursive: true, force: true });
});

async function login(page: Page, username: string) {
  // Feature cases reuse validated sessions; the login/logout case still exercises the UI.
  const cookies = sessionCookies.get(username);
  if (cookies) {
    await page.context().addCookies(cookies);
    const response = await page.request.get(`${baseURL}/api/v1/auth/me`);
    if (response.ok() && (await response.json()).user.username === username) {
      await page.goto(baseURL);
      await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
      return;
    }
    await page.context().clearCookies();
  }
  await page.goto(baseURL);
  await page.getByLabel('Username', { exact: true }).fill(username);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
  sessionCookies.set(username, await page.context().cookies(baseURL));
}

async function expandImport(page: Page, id: string, history = false) {
  if (history) {
    const historyPanel = page.getByTestId('import-history');
    await expect(historyPanel).toBeVisible();
    if (await historyPanel.getAttribute('open') === null) {
      await historyPanel.locator(':scope > summary').click();
    }
  }
  const row = page.locator(`[data-testid="${history ? 'history-row' : 'attention-row'}"][data-job-id="${id}"]`);
  const details = row.locator(':scope > details.import-details');
  await expect(details).toBeVisible();
  if (await details.getAttribute('open') === null) {
    await details.locator(':scope > summary').click();
  }
  return row;
}

test('browser login, search, playback, seek, per-user resume and logout', async ({ page, browser }) => {
  await login(page, 'e2e-admin');
  await page.getByRole('searchbox', { name: 'Search your library' }).fill('missing-title');
  await expect(page.getByRole('heading', { name: 'No matching movies' })).toBeVisible();
  await page.getByRole('searchbox', { name: 'Search your library' }).fill('E2E');
  await page.getByRole('button', { name: `Watch ${title}`, exact: true }).click();
  const video = page.getByTestId('movie-player');
  await expect(video).toBeVisible();
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.readyState)).toBeGreaterThanOrEqual(1);
  await video.evaluate(async (element: HTMLVideoElement) => {
    element.muted = true;
    await element.play();
  });
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.currentTime)).toBeGreaterThan(0.25);
  await video.evaluate((element: HTMLVideoElement) => {
    element.pause();
    element.currentTime = 4;
  });
  await expect.poll(async () => {
    const response = await page.request.get(`${baseURL}/api/v1/movies/${movieId}/progress`);
    return (await response.json()).position_seconds;
  }).toBeGreaterThan(3.5);
  await page.getByRole('button', { name: 'Back to library', exact: true }).click();
  await page.reload();
  await page.getByRole('button', { name: `Watch ${title}`, exact: true }).click();
  await expect.poll(() => page.getByTestId('movie-player').evaluate((element: HTMLVideoElement) => element.currentTime)).toBeGreaterThan(3.5);

  const viewer = await browser.newPage();
  try {
    await login(viewer, 'e2e-viewer');
    await viewer.getByRole('button', { name: `Watch ${title}`, exact: true }).click();
    await expect.poll(() => viewer.getByTestId('movie-player').evaluate((element: HTMLVideoElement) => element.readyState)).toBeGreaterThanOrEqual(1);
    expect(await viewer.getByTestId('movie-player').evaluate((element: HTMLVideoElement) => element.currentTime)).toBeLessThan(0.5);
    const forbidden = await viewer.request.get(`${baseURL}/api/v1/admin/incoming`);
    expect(forbidden.status()).toBe(403);
  } finally {
    await viewer.close();
  }
  const resumedVideo = page.getByTestId('movie-player');
  await resumedVideo.evaluate(async (element: HTMLVideoElement) => {
    element.muted = true;
    element.currentTime = element.duration - 0.25;
    await element.play();
  });
  await expect.poll(() => resumedVideo.evaluate((element: HTMLVideoElement) => element.ended)).toBe(true);
  await page.getByRole('button', { name: 'Back to library', exact: true }).click();
  await expect(page.getByRole('button', { name: `Watch ${title}`, exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Sign out', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign in', exact: true })).toBeVisible();
  expect((await page.request.get(`${baseURL}/api/v1/movies/${movieId}/video`)).status()).toBe(401);
});

test('authenticated media requests support byte ranges and reject missing sessions', async ({ page, browser }) => {
  const anonymous = await browser.newContext();
  try {
    expect((await anonymous.request.get(`${baseURL}/api/v1/movies/${movieId}/video`)).status()).toBe(401);
  } finally {
    await anonymous.close();
  }
  await login(page, 'e2e-viewer');
  const response = await page.request.get(`${baseURL}/api/v1/movies/${movieId}/video`, { headers: { Range: 'bytes=0-31' } });
  expect(response.status()).toBe(206);
  expect(response.headers()['content-range']).toMatch(/^bytes 0-31\/\d+$/);
  expect((await response.body()).length).toBe(32);
  const invalid = await page.request.get(`${baseURL}/api/v1/movies/${movieId}/video`, { headers: { Range: 'bytes=999999999999-' } });
  expect(invalid.status()).toBe(416);
});

test('movie Trash hides new playback, preserves active ranged playback and restores the library', async ({ page, browser }) => {
  await login(page, 'e2e-admin');
  const viewer = await browser.newPage();
  try {
    await login(viewer, 'e2e-viewer');
    await expect(viewer.getByRole('button', { name: 'Move to Trash', exact: true })).toHaveCount(0);
    await viewer.getByRole('button', { name: `Watch ${title}`, exact: true }).click();
    const video = viewer.getByTestId('movie-player');
    await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.readyState)).toBeGreaterThanOrEqual(1);
    const activeURL = await video.getAttribute('src');
    expect(activeURL).toContain('?playback=');
    await page.getByRole('button', { name: 'Move to Trash', exact: true }).click();
    await expect(page.getByText('Remove from the library?', { exact: false })).toBeVisible();
    await page.getByRole('button', { name: 'Cancel', exact: true }).click();
    await expect(page.getByRole('button', { name: `Watch ${title}`, exact: true })).toBeVisible();
    await page.getByRole('button', { name: 'Move to Trash', exact: true }).click();
    await page.getByRole('button', { name: 'Confirm move to Trash', exact: true }).click();
    await expect(page.getByRole('button', { name: `Watch ${title}`, exact: true })).toHaveCount(0);
    expect((await viewer.request.get(`${baseURL}/api/v1/movies/${movieId}`)).status()).toBe(404);
    expect((await viewer.request.get(`${baseURL}/api/v1/movies/${movieId}/video`)).status()).toBe(404);
    const active = await viewer.request.get(`${baseURL}${activeURL}`, { headers: { Range: 'bytes=0-31' } });
    expect(active.status()).toBe(206);
    expect((await active.body()).length).toBe(32);
    expect((await viewer.request.get(`${baseURL}/api/v1/admin/trash`)).status()).toBe(403);
    await page.getByText('Trash', { exact: true }).click();
    await expect(page.getByText(/Restore before /)).toBeVisible();
    await page.reload();
    await page.getByText('Trash', { exact: true }).click();
    await page.getByRole('button', { name: 'Restore movie', exact: true }).click();
    await expect(page.getByRole('button', { name: `Watch ${title}`, exact: true })).toBeVisible();
    await expect(page.getByText('Trash is empty.', { exact: true })).toBeVisible();
  } finally {
    const session = await (await page.request.get(`${baseURL}/api/v1/auth/me`)).json();
    await page.request.post(`${baseURL}/api/v1/admin/movies/${movieId}/restore`, {
      headers: { Origin: baseURL, 'x-csrf-token': session.csrf_token },
    });
    await viewer.close();
  }
});

test('failed resume save still allows returning to the library with a warning', async ({ page }) => {
  await login(page, 'e2e-viewer');
  await page.route('**/api/v1/movies/*/progress*', async route => {
    if (route.request().method() === 'PUT') {
      await route.fulfill({ status: 500, contentType: 'application/json', body: '{"error":"Injected save failure"}' });
    } else {
      await route.continue();
    }
  });
  await page.getByRole('button', { name: `Watch ${title}`, exact: true }).click();
  const video = page.getByTestId('movie-player');
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.readyState)).toBeGreaterThanOrEqual(1);
  await video.evaluate((element: HTMLVideoElement) => { element.currentTime = 2; });
  await page.getByRole('button', { name: 'Back to library', exact: true }).click();
  await expect(page.getByRole('button', { name: `Watch ${title}`, exact: true })).toBeVisible();
  await expect(page.getByRole('alert')).toContainText(/resume|playback position/i);
  await page.waitForResponse(response => response.url().endsWith('/api/v1/movies') && response.request().method() === 'GET');
  await expect(page.getByRole('alert')).toContainText(/resume|playback position/i);
});

test('admin diagnostics and ISO scanning retain failed input without publication', async ({ page }) => {
  await login(page, 'e2e-admin');
  await expect(page.getByText('Worker connected', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Check again', exact: true }).click();
  await expect.poll(async () => {
    const jobs = await (await page.request.get(`${baseURL}/api/v1/jobs`)).json();
    return jobs.some(job => job.state === 'succeeded' || job.state === 'failed');
  }).toBe(true);
  await page.getByTestId('latest-readiness').getByText('Individual checks', { exact: true }).click();
  await expect(page.getByTestId('latest-readiness').getByText('Required tools are available. OCR is optional.', { exact: true })).toBeVisible();
  const original = (await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json()).files.find(file => file.name === 'browser-invalid.iso');
  const originalHash = command('sha256sum', [incomingFile]).split(/\s+/, 1)[0];
  await page.getByRole('button', { name: 'Refresh incoming files', exact: true }).click();
  await page.getByLabel('Completed ISO copy', { exact: true }).selectOption({ index: 1 });
  await page.getByRole('checkbox', { name: /I confirm this copy is finished/ }).check();
  await page.getByRole('button', { name: 'Scan DVD', exact: true }).click();
  await expect.poll(async () => {
    const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
    return jobs.some(job => job.name === 'browser-invalid.iso' && job.state === 'failed');
  }).toBe(true);
  const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
  const failed = jobs.find(job => job.name === 'browser-invalid.iso');
  await expandImport(page, failed.id);
  await expect(page.getByText('Scan failed — ISO, reports and logs retained', { exact: true })).toBeVisible();
  expect(failed.attempt_id).toBeTruthy();
  expect(command('sha256sum', [join(artifactsRoot!, 'scratch', failed.attempt_id, 'input.iso')]).split(/\s+/, 1)[0]).toBe(originalHash);
  expect(command('sha256sum', [incomingFile]).split(/\s+/, 1)[0]).toBe(originalHash);
  const retained = (await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json()).files.find(file => file.name === original.name);
  expect(retained.identity).toBe(original.identity);
  expect((await (await page.request.get(`${baseURL}/api/v1/movies`)).json()).movies).toHaveLength(1);
});

test('scanned title and angle choices gate conversion and explicit publication through mocked contracts', async ({ page }) => {
  const scanId = randomUUID();
  const probeId = randomUUID();
  const conversionId = randomUUID();
  const digest = 'a'.repeat(64);
  const scan = {
    id: scanId, name: 'browser-invalid.iso', state: 'needs_review',
    created_at: Math.floor(Date.now() / 1000), attempt_id: randomUUID(), error: null,
    report: { titles: [
      { title: 1, duration_seconds: 8, chapter_count: 1, audio_count: 1, subtitle_count: 0,
        audio_languages: ['en'], subtitle_languages: [], angles: [1], main_feature_candidate: false },
      { title: 2, duration_seconds: 60, chapter_count: 3, audio_count: 2, subtitle_count: 1,
        audio_languages: ['en', 'fr'], subtitle_languages: ['en'], angles: [1, 2], main_feature_candidate: true },
    ] },
  };
  const probe = {
    id: probeId, name: scan.name, title: 2, angle: 2, state: 'needs_review',
    created_at: scan.created_at, attempt_id: scan.attempt_id, error: null,
    report: { sha256: digest, inspection: { warnings: [], metadata: {
      format: { duration: '60' },
      streams: [
        { codec_type: 'video', codec_name: 'mpeg2video', width: 720, height: 480 },
        { codec_type: 'audio', codec_name: 'ac3', channels: 2, tags: { language: 'en' } },
      ],
    } } },
  };
  let inspected = false;
  let conversionState: string | undefined;
  let conversionPolls = 0;
  const conversion = () => ({
    id: conversionId, probe_job_id: probeId, state: conversionState,
    created_at: scan.created_at, attempt_id: randomUUID(), video_treatment: 'yadif',
    phase: conversionState === 'needs_review' ? 'awaiting_review' : conversionState,
    error: null, movie_id: conversionState === 'published' ? movieId : null,
    movie_sha256: conversionState === 'needs_review' || conversionState === 'published' ? digest : null,
    report: {
      experimental: true, published: false, source_deletion_enabled: false,
      status: 'verified_experimental_output', warnings: [],
      profile: { video_treatment: 'yadif' },
      validation: { source_decoded_frames: 192, output_decoded_frames: 192,
        source_decoded_seconds: 8, output_decoded_seconds: 8,
        movie: { format: { duration: '8' }, streams: [
          { codec_type: 'video', codec_name: 'h264', width: 160, height: 90, pix_fmt: 'yuv420p' },
          { codec_type: 'audio', codec_name: 'aac', channels: 2 },
        ] } },
    },
  });
  await page.route('**/api/v1/admin/imports?*', route => {
    const history = new URL(route.request().url()).searchParams.get('history') === 'true';
    if (!history) {
      if (conversionState === 'queued' || conversionState === 'converting') {
        conversionState = ++conversionPolls >= 2 ? 'needs_review' : 'converting';
      } else if (conversionState === 'queued_publication') {
        conversionState = 'published';
      }
    }
    const entries = [
      { item: { kind: 'scan', job: scan }, can_archive: inspected, history: inspected },
      ...(inspected ? [{ item: { kind: 'probe', job: probe }, can_archive: !!conversionState, history: !!conversionState }] : []),
      ...(conversionState ? [{ item: { kind: 'conversion', job: conversion() },
        can_archive: conversionState === 'published', history: conversionState === 'published' }] : []),
    ];
    const filtered = entries.filter(entry => entry.history === history);
    return route.fulfill({ json: {
      entries: filtered, total: filtered.length, history_count: entries.filter(entry => entry.history).length,
    } });
  });
  await page.route(`**/api/v1/admin/scans/${scanId}/inspect`, async route => {
    expect(route.request().postDataJSON()).toEqual({ title: 2, angle: 2 });
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    inspected = true;
    await route.fulfill({ status: 202, json: probe });
  });
  await page.route('**/api/v1/admin/probes', route => route.fulfill({ json: inspected ? [probe] : [] }));
  await page.route('**/api/v1/admin/conversions', async route => {
    if (route.request().method() === 'POST') {
      expect(route.request().postDataJSON()).toEqual({ probe_job_id: probeId, video_treatment: 'yadif' });
      expect(route.request().headers()['x-csrf-token']).toBeTruthy();
      conversionState = 'queued';
      await route.fulfill({ status: 202, json: conversion() });
    } else {
      if (conversionState === 'queued' || conversionState === 'converting') {
        conversionState = ++conversionPolls >= 2 ? 'needs_review' : 'converting';
      } else if (conversionState === 'queued_publication') {
        conversionState = 'published';
      }
      await route.fulfill({ json: { jobs: conversionState ? [conversion()] : [] } });
    }
  });
  await page.route(`**/api/v1/admin/conversions/${conversionId}/publish`, async route => {
    expect(route.request().postDataJSON()).toEqual({
      title: 'Reviewed DVD title', year: 2002, description: 'Reviewed metadata',
      movie_sha256: digest, confirm: true,
    });
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    conversionState = 'queued_publication';
    await route.fulfill({ status: 202, json: conversion() });
  });

  await login(page, 'e2e-admin');
  await expandImport(page, scanId);
  const scanCard = page.locator(`[data-testid="scan-job"][data-job-id="${scanId}"]`);
  const titleSelect = scanCard.getByRole('combobox', { name: 'DVD title', exact: true });
  await expect(titleSelect.locator('option')).toHaveCount(3);
  await titleSelect.selectOption('1');
  await expect(scanCard.getByRole('combobox', { name: 'DVD angle', exact: true })).toHaveCount(0);
  await titleSelect.selectOption('2');
  const angleSelect = scanCard.getByRole('combobox', { name: 'DVD angle', exact: true });
  await expect(angleSelect.locator('option')).toHaveCount(3);
  await angleSelect.selectOption('2');
  await scanCard.getByRole('button', { name: 'Inspect selected title', exact: true }).click();

  const probeCard = page.locator(`[data-testid="probe-job"][data-job-id="${probeId}"]`);
  await expandImport(page, probeId);
  const convert = probeCard.getByRole('button', { name: 'Convert reviewed title', exact: true });
  await expect(convert).toBeDisabled();
  await probeCard.getByRole('combobox', { name: 'Video treatment', exact: true }).selectOption('yadif');
  await probeCard.getByRole('checkbox', { name: /I reviewed this inspection and warnings/ }).check();
  await convert.click();

  const conversionCard = page.locator(`[data-testid="conversion-job"][data-job-id="${conversionId}"]`);
  await expandImport(page, conversionId);
  await expect(conversionCard.getByText('Conversion needs review — not published', { exact: true })).toBeVisible();
  const publish = conversionCard.getByRole('button', { name: 'Publish to local library', exact: true });
  await expect(publish).toBeDisabled();
  await conversionCard.getByRole('textbox', { name: 'Movie title', exact: true }).fill('Reviewed DVD title');
  await conversionCard.getByRole('spinbutton', { name: 'Year (optional)', exact: true }).fill('2002');
  await conversionCard.getByRole('textbox', { name: 'Description', exact: true }).fill('Reviewed metadata');
  await expect(publish).toBeDisabled();
  await conversionCard.getByRole('checkbox', { name: /I reviewed the validated conversion, warnings and metadata/ }).check();
  await page.waitForResponse(response => response.url().includes('/api/v1/admin/imports?') && response.request().method() === 'GET');
  await expect(conversionCard.getByRole('textbox', { name: 'Movie title', exact: true })).toHaveValue('Reviewed DVD title');
  await publish.click();
  await expandImport(page, conversionId, true);
  await expect(conversionCard.getByText('Published to the local library', { exact: true })).toBeVisible();
  await conversionCard.getByRole('button', { name: 'Watch published movie', exact: true }).click();
  await expect(page.getByTestId('movie-player')).toBeVisible();
});

for (const clockSkew of [0, -5_000, 2 * 86_400_000]) {
  test(`TMDB metadata lookup requires explicit selection and editable digest-bound publication (browser clock ${clockSkew}ms)`, async ({ page }) => {
    const pageErrors: string[] = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    await page.addInitScript(({ offset }) => {
      const now = Date.now.bind(Date);
      Date.now = () => now() + offset;
    }, { offset: clockSkew });
    const id = randomUUID();
    const digest = 'd'.repeat(64);
    const fetchedAt = Math.floor(Date.now() / 1000);
    const conversion = {
      id, probe_job_id: randomUUID(), state: 'needs_review', created_at: fetchedAt,
      attempt_id: randomUUID(), video_treatment: 'preserve', phase: 'awaiting_review',
      error: null, movie_id: null, movie_sha256: digest,
      report: { status: 'verified_experimental_output', movie_sha256: digest, warnings: [] },
    };
    let published = false;
    let searches = 0;
    let releasePublication!: () => void;
    const publicationGate = new Promise<void>(resolve => { releasePublication = resolve; });
    await page.route('**/api/v1/admin/imports?*', route => {
      const history = new URL(route.request().url()).searchParams.get('history') === 'true';
      const entries = history ? [] : [{ item: { kind: 'conversion', job: conversion }, can_archive: false, history: false }];
      return route.fulfill({ json: { entries, total: entries.length, history_count: 0 } });
    });
    await page.route('**/api/v1/admin/metadata', route => route.fulfill({ json: { configured: true } }));
    await page.route('**/api/v1/admin/metadata/search', async route => {
      searches++;
      expect(route.request().method()).toBe('POST');
      expect(route.request().headers()['x-csrf-token']).toBeTruthy();
      const request = route.request().postDataJSON();
      expect(Object.keys(request).sort()).toEqual(['query', 'year']);
      if (request.query === 'offline') {
        return route.fulfill({ status: 502, json: { error: 'TMDB is unavailable. Try again later or enter metadata manually.' } });
      }
      if (request.query === 'none') return route.fulfill({ json: { movies: [] } });
      if (request.query === 'Fresh search') {
        expect(request.year).toBe(2002);
        return route.fulfill({ json: { movies: [
          { id: 3, title: 'Another movie', year: 2003, description: 'Another synopsis', fetched_at: fetchedAt },
        ] } });
      }
      expect(request).toEqual({ query: 'Monte Cristo', year: 2002 });
      await new Promise(resolve => setTimeout(resolve, 300));
      return route.fulfill({ json: { movies: [
        { id: 1, title: 'The Count of Monte Cristo', year: 1934, description: 'First adaptation', fetched_at: fetchedAt },
        { id: 2, title: 'The Count of Monte Cristo', year: 2002, description: 'Selected synopsis', fetched_at: fetchedAt },
      ] } });
    });
    await page.route(`**/api/v1/admin/conversions/${id}/publish`, async route => {
      expect(route.request().postDataJSON()).toEqual({
        title: 'My reviewed title', year: 2003, description: 'Edited synopsis',
        movie_sha256: digest, confirm: true, tmdb: { id: 3, fetched_at: fetchedAt },
      });
      expect(route.request().headers()['x-csrf-token']).toBeTruthy();
      published = true;
      await publicationGate;
      return route.fulfill({ status: 202, json: { ...conversion, state: 'queued_publication' } });
    });
    await login(page, 'e2e-admin');
    await expandImport(page, id);
    const card = page.locator(`[data-testid="conversion-job"][data-job-id="${id}"]`);
    const titleField = card.getByRole('textbox', { name: 'Movie title', exact: true });
    const description = card.getByRole('textbox', { name: 'Description', exact: true });
    const search = card.getByRole('textbox', { name: 'Search TMDB title', exact: true });
    const searchYear = card.getByRole('spinbutton', { name: 'Search TMDB year (optional)', exact: true });
    const searchButton = card.getByRole('button', { name: 'Search TMDB', exact: true });
    const confirmation = card.getByRole('checkbox', { name: /I reviewed the validated conversion/ });
    const publish = card.getByRole('button', { name: 'Publish to local library', exact: true });
    const matches = card.getByRole('button', { name: 'Use this match', exact: true });
    const selected = card.getByRole('region', { name: 'Selected TMDB match', exact: true });
    const changeMatch = card.getByRole('button', { name: 'Change match', exact: true });
    const yearField = card.getByRole('spinbutton', { name: 'Year (optional)', exact: true });
    await expect(card.locator('form form')).toHaveCount(0);
    await titleField.fill('Manual fallback');
    await confirmation.check();
    await search.fill('offline');
    await searchButton.click();
    await expect(card.getByText('TMDB is unavailable. Try again later or enter metadata manually.')).toBeVisible();
    await expect(titleField).toHaveValue('Manual fallback');
    await expect(publish).toBeEnabled();
    await search.fill('none');
    await searchButton.click();
    await expect(card.getByText('No matches; try another search or enter metadata manually.')).toBeVisible();
    await search.fill('Monte Cristo');
    await searchYear.fill('2002');
    await searchButton.click();
    await titleField.fill('Edited while searching');
    await expect(matches).toHaveCount(2);
    await expect(matches.first()).toHaveAttribute('type', 'button');
    await expect(titleField).toHaveValue('Edited while searching');
    expect(published).toBe(false);
    await matches.nth(1).click();
    await expect(titleField).toHaveValue('The Count of Monte Cristo');
    await expect(yearField).toHaveValue('2002');
    await expect(description).toHaveValue('Selected synopsis');
    await expect(matches).toHaveCount(0);
    await expect(selected).toContainText('The Count of Monte Cristo (2002)');
    await expect(selected).toContainText('Source: TMDB');
    await expect(card.getByText('The Count of Monte Cristo (1934)', { exact: true })).toHaveCount(0);
    await expect(changeMatch).toHaveAttribute('type', 'button');
    await expect(confirmation).not.toBeChecked();
    await expect(publish).toBeDisabled();
    await titleField.fill('Keep this edit');
    await confirmation.check();
    const details = page.locator(`[data-testid="attention-row"][data-job-id="${id}"] > details.import-details`);
    await details.locator(':scope > summary').click();
    await expect(selected).not.toBeVisible();
    await expandImport(page, id);
    await expect(selected).toBeVisible();
    await expect(matches).toHaveCount(0);
    await page.waitForResponse(response => response.url().includes('/api/v1/admin/imports?') && response.request().method() === 'GET');
    await expect(matches).toHaveCount(0);
    await expect(selected).toContainText('The Count of Monte Cristo (2002)');
    await changeMatch.click();
    await expect(matches).toHaveCount(2);
    await expect(titleField).toHaveValue('Keep this edit');
    await expect(confirmation).toBeChecked();
    expect(searches).toBe(3);
    await matches.first().click();
    await expect(yearField).toHaveValue('1934');
    await expect(description).toHaveValue('First adaptation');
    await expect(confirmation).not.toBeChecked();
    await expect(selected).toContainText('The Count of Monte Cristo (1934)');
    await expect(matches).toHaveCount(0);
    await card.getByRole('button', { name: 'Clear TMDB selection for manual entry' }).click();
    await expect(titleField).toHaveValue('');
    await expect(description).toHaveValue('');
    await expect(yearField).toHaveValue('');
    await expect(selected).toHaveCount(0);
    await matches.nth(1).click();
    await titleField.fill('Keep while searching');
    await confirmation.check();
    await search.fill('Fresh search');
    await searchButton.click();
    await expect(matches).toHaveCount(1);
    await expect(titleField).toHaveValue('Keep while searching');
    await expect(confirmation).toBeChecked();
    await expect(selected).toContainText('The Count of Monte Cristo (2002)');
    await matches.click();
    await expect(titleField).toHaveValue('Another movie');
    await expect(yearField).toHaveValue('2003');
    await expect(description).toHaveValue('Another synopsis');
    await expect(confirmation).not.toBeChecked();
    await expect(selected).toContainText('Another movie (2003)');
    await expect(matches).toHaveCount(0);
    await confirmation.check();
    await titleField.fill('My reviewed title');
    await expect(confirmation).not.toBeChecked();
    await description.fill('Edited synopsis');
    await confirmation.check();
    await page.waitForResponse(response => response.url().includes('/api/v1/admin/imports?') && response.request().method() === 'GET');
    await expect(titleField).toHaveValue('My reviewed title');
    await expect(description).toHaveValue('Edited synopsis');
    await expect(confirmation).toBeChecked();
    await expect(matches).toHaveCount(0);
    await publish.click();
    await expect(changeMatch).toBeDisabled();
    await expect(matches).toHaveCount(0);
    await expect.poll(() => published).toBe(true);
    releasePublication();
    await expect(card.getByRole('heading', { name: 'Optional TMDB lookup', exact: true })).toHaveCount(0);
    expect(searches).toBe(4);
    const logo = page.getByRole('img', { name: 'TMDB', exact: true });
    await expect(logo).toHaveAttribute('src', '/tmdb.svg');
    await expect(page.getByText('This application uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.')).toBeVisible();
    expect(pageErrors).toEqual([]);
  });
}

test('TMDB stale publication and rebuilt review offer search recovery and manual entry', async ({ page }) => {
  const pageErrors: string[] = [];
  page.on('pageerror', error => pageErrors.push(error.message));
  const id = randomUUID();
  const digest = 'e'.repeat(64);
  const fetchedAt = Math.floor(Date.now() / 1000);
  const conversion = {
    id, probe_job_id: randomUUID(), state: 'needs_review', created_at: fetchedAt,
    attempt_id: randomUUID(), video_treatment: 'preserve', phase: 'awaiting_review',
    error: null, movie_id: null, movie_sha256: digest,
    report: { status: 'verified_experimental_output', movie_sha256: digest, warnings: [] },
  };
  let attempts = 0;
  let searches = 0;
  await page.route('**/api/v1/admin/imports?*', route => {
    const history = new URL(route.request().url()).searchParams.get('history') === 'true';
    const entries = history ? [] : [{ item: { kind: 'conversion', job: conversion }, can_archive: false, history: false }];
    return route.fulfill({ json: { entries, total: entries.length, history_count: 0 } });
  });
  await page.route('**/api/v1/admin/metadata', route => route.fulfill({ json: { configured: true } }));
  await page.route('**/api/v1/admin/metadata/search', route => {
    searches++;
    return route.fulfill({ json: { movies: [
      { id: 7, title: 'Synthetic match', year: 2002, description: 'Synthetic synopsis', fetched_at: fetchedAt },
    ] } });
  });
  await page.route(`**/api/v1/admin/conversions/${id}/publish`, route => {
    const body = route.request().postDataJSON();
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    attempts++;
    if (attempts === 1) {
      expect(body.tmdb).toEqual({ id: 7, fetched_at: fetchedAt });
      return route.fulfill({ status: 409, json: { error: 'TMDB selection expired; search again before publication' } });
    }
    expect(body).toEqual({
      title: 'Manual recovery', year: null, description: '',
      movie_sha256: digest, confirm: true,
    });
    return route.fulfill({ status: 202, json: { ...conversion, state: 'queued_publication' } });
  });
  await login(page, 'e2e-admin');
  await expandImport(page, id);
  const card = page.locator(`[data-testid="conversion-job"][data-job-id="${id}"]`);
  const searchButton = card.getByRole('button', { name: 'Search TMDB', exact: true });
  const search = card.getByRole('textbox', { name: 'Search TMDB title', exact: true });
  const searchAgain = card.getByRole('button', { name: 'Search again', exact: true });
  const changeMatch = card.getByRole('button', { name: 'Change match', exact: true });
  const matches = card.getByRole('button', { name: 'Use this match', exact: true });
  const selected = card.getByRole('region', { name: 'Selected TMDB match', exact: true });
  const titleField = card.getByRole('textbox', { name: 'Movie title', exact: true });
  const confirmation = card.getByRole('checkbox', { name: /I reviewed the validated conversion/ });
  const publish = card.getByRole('button', { name: 'Publish to local library', exact: true });
  await search.fill('Synthetic search');
  await searchButton.click();
  await matches.click();
  await titleField.fill('Keep through review rebuild');
  await confirmation.check();
  conversion.movie_sha256 = '';
  await expect(search).toHaveCount(0);
  conversion.movie_sha256 = digest;
  await expect(selected).toContainText('Synthetic match (2002)');
  await expect(titleField).toHaveValue('Keep through review rebuild');
  await expect(confirmation).not.toBeChecked();
  await expect(changeMatch).toHaveCount(0);
  await expect(searchAgain).toHaveAttribute('type', 'button');
  await searchAgain.click();
  await expect(search).toBeFocused();
  await expect(titleField).toHaveValue('Keep through review rebuild');
  await expect(selected).toContainText('Source: TMDB');
  expect(searches).toBe(1);
  await search.fill('Synthetic search');
  await searchButton.click();
  await expect(matches).toHaveCount(1);
  await expect(titleField).toHaveValue('Keep through review rebuild');
  await matches.click();
  await expect(changeMatch).toBeVisible();
  await expect(matches).toHaveCount(0);
  await confirmation.check();
  await publish.click();
  await expect(card.getByText('TMDB selection expired; search again before publication', { exact: true })).toBeVisible();
  await expect(confirmation).not.toBeChecked();
  await expect(publish).toBeDisabled();
  await expect(matches).toHaveCount(0);
  await expect(searchButton).toBeEnabled();
  await expect(changeMatch).toHaveCount(0);
  await expect(searchAgain).toBeVisible();
  await searchAgain.click();
  await expect(search).toBeFocused();
  await expect(titleField).toHaveValue('Synthetic match');
  await expect(selected).toContainText('Synthetic match (2002)');
  expect(searches).toBe(2);
  await expect(publish).toBeDisabled();
  expect(attempts).toBe(1);
  await searchButton.click();
  await expect(matches).toHaveCount(1);
  await expect(selected).toContainText('Synthetic match (2002)');
  await expect(publish).toBeDisabled();
  expect(searches).toBe(3);
  await card.getByRole('button', { name: 'Clear TMDB selection for manual entry', exact: true }).click();
  await titleField.fill('Manual recovery');
  await confirmation.check();
  await publish.click();
  await expect.poll(() => attempts).toBe(2);
  expect(pageErrors).toEqual([]);
});

test('movie labels persist through polling, reuse and removal, with viewer search and exact filters', async ({ page, browser }) => {
  await login(page, 'e2e-admin');
  const session = await (await page.request.get(`${baseURL}/api/v1/auth/me`)).json();
  const headers = { origin: baseURL, 'x-csrf-token': session.csrf_token };
  const labelsUrl = `${baseURL}/api/v1/admin/movies/${movieId}/labels`;
  const empty = { categories: [], tags: [] };
  let library = await (await page.request.get(`${baseURL}/api/v1/movies`)).json();
  let movie = library.movies.find((movie: { id: string }) => movie.id === movieId);
  const reset = await page.request.post(labelsUrl, { headers, data: {
    expected: { categories: movie.categories, tags: movie.tags }, labels: empty,
  } });
  expect(reset.ok()).toBeTruthy();
  await page.reload();
  const card = page.locator('article.movie').filter({ has: page.getByRole('heading', { name: title, exact: true }) });
  await card.getByRole('button', { name: 'Edit labels', exact: true }).click();
  const editor = page.getByRole('region', { name: 'Edit movie labels', exact: true });
  await editor.getByRole('textbox', { name: 'Categories', exact: true }).fill(' Family   night \nDRAMA\ndrama');
  await editor.getByRole('textbox', { name: 'Tags', exact: true }).fill('Special shelf\nspecial shelf');
  await page.waitForResponse(response => response.url().endsWith('/api/v1/movies'));
  await expect(editor.getByRole('textbox', { name: 'Categories', exact: true })).toHaveValue(' Family   night \nDRAMA\ndrama');
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor).toHaveCount(0);
  await expect(card.getByText('Categories: DRAMA, Family night', { exact: true })).toBeVisible();
  library = await (await page.request.get(`${baseURL}/api/v1/movies`)).json();
  movie = library.movies.find((movie: { id: string }) => movie.id === movieId);
  expect(movie.tags).toEqual(['Special shelf']);
  await card.getByRole('button', { name: 'Edit labels', exact: true }).click();
  await editor.getByRole('textbox', { name: 'Categories', exact: true }).fill('');
  await editor.getByLabel('Reuse Categories', { exact: true }).selectOption('Family night');
  await editor.getByRole('textbox', { name: 'Tags', exact: true }).fill('');
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor).toHaveCount(0);
  await page.reload();
  await expect(card.getByText('Categories: Family night', { exact: true })).toBeVisible();
  await expect(card.getByText('Tags:', { exact: true })).toHaveCount(0);
  const viewer = await browser.newPage();
  try {
    await login(viewer, 'e2e-viewer');
    await expect(viewer.getByRole('button', { name: 'Edit labels', exact: true })).toHaveCount(0);
    await viewer.getByLabel('Search your library', { exact: true }).fill('family night');
    await expect(viewer.getByRole('heading', { name: title, exact: true })).toBeVisible();
    await viewer.getByLabel('Filter by category', { exact: true }).selectOption('Family night');
    await viewer.getByLabel('Search your library', { exact: true }).fill('does not match');
    await expect(viewer.getByRole('heading', { name: 'No matching movies', exact: true })).toBeVisible();
    await viewer.getByLabel('Search your library', { exact: true }).fill('');
    await viewer.waitForResponse(response => response.url().endsWith('/api/v1/movies'));
    await expect(viewer.getByLabel('Filter by category', { exact: true })).toHaveValue('Family night');
    await expect(viewer.getByRole('heading', { name: title, exact: true })).toBeVisible();
    const viewerSession = await (await viewer.request.get(`${baseURL}/api/v1/auth/me`)).json();
    const denied = await viewer.request.post(labelsUrl, { headers: {
      origin: baseURL, 'x-csrf-token': viewerSession.csrf_token,
    }, data: { expected: empty, labels: empty } });
    expect(denied.status()).toBe(403);
  } finally {
    await viewer.close();
  }
  // An out-of-date editor must not overwrite another administrator's labels.
  const beforeOpen = await page.request.post(labelsUrl, { headers, data: {
    expected: { categories: ['Family night'], tags: [] },
    labels: { categories: ['Family night'], tags: ['Changed before opening'] },
  } });
  expect(beforeOpen.ok()).toBeTruthy();
  await card.getByRole('button', { name: 'Edit labels', exact: true }).click();
  await expect(editor.getByRole('textbox', { name: 'Tags', exact: true })).toHaveValue('Changed before opening');
  await editor.getByRole('textbox', { name: 'Tags', exact: true }).fill('Unsaved draft');
  const external = await page.request.post(labelsUrl, { headers, data: {
    expected: { categories: ['Family night'], tags: ['Changed before opening'] },
    labels: { categories: ['Family night'], tags: ['Changed elsewhere'] },
  } });
  expect(external.ok()).toBeTruthy();
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor.getByRole('alert')).toContainText('Labels changed elsewhere. Review the latest labels and save again.');
  await expect(editor.getByRole('region', { name: 'Latest saved labels', exact: true })).toContainText('Changed elsewhere');
  await expect(editor.getByRole('textbox', { name: 'Tags', exact: true })).toHaveValue('Unsaved draft');
  await editor.getByRole('textbox', { name: 'Tags', exact: true }).fill('Changed elsewhere\nUnsaved draft');
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor).toHaveCount(0);
  await expect(card.getByText('Tags: Changed elsewhere, Unsaved draft', { exact: true })).toBeVisible();
  const cleanup = await page.request.post(labelsUrl, { headers, data: {
    expected: { categories: ['Family night'], tags: ['Changed elsewhere', 'Unsaved draft'] }, labels: empty,
  } });
  expect(cleanup.ok()).toBeTruthy();
});

test('movie label saves reject delayed older library polls without clobbering reopened drafts', async ({ page }) => {
  const movie = {
    id: movieId, title: 'Polling movie', year: null, description: '',
    categories: ['Original'], tags: ['Old tag'],
  };
  let polls = 0;
  let heldPollStarted = false;
  let releasePoll!: () => void;
  const heldPoll = new Promise<void>(resolve => { releasePoll = resolve; });
  await page.route('**/api/v1/movies', async route => {
    const snapshot = { ...movie, categories: [...movie.categories], tags: [...movie.tags] };
    polls++;
    if (polls === 2) {
      heldPollStarted = true;
      await heldPoll;
    }
    await route.fulfill({ json: { movies: [snapshot] } });
  });
  await page.route(`**/api/v1/movies/${movieId}/labels`, route => route.fulfill({
    json: { categories: movie.categories, tags: movie.tags },
  }));
  await page.route('**/api/v1/labels', route => route.fulfill({
    json: { categories: movie.categories, tags: movie.tags },
  }));
  await page.route(`**/api/v1/admin/movies/${movieId}/labels`, async route => {
    const request = route.request().postDataJSON();
    expect(request.expected).toEqual({ categories: movie.categories, tags: movie.tags });
    movie.categories = request.labels.categories;
    movie.tags = request.labels.tags;
    await route.fulfill({ json: request.labels });
  });
  await login(page, 'e2e-admin');
  const card = page.locator('article.movie');
  await card.getByRole('button', { name: 'Edit labels', exact: true }).click();
  const editor = page.getByRole('region', { name: 'Edit movie labels', exact: true });
  await editor.getByRole('textbox', { name: 'Tags', exact: true }).fill('Saved now');
  await expect.poll(() => heldPollStarted).toBe(true);
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor).toHaveCount(0);
  await expect(card.getByText('Tags: Saved now', { exact: true })).toBeVisible();
  const completedPoll = page.waitForResponse(response => response.url().endsWith('/api/v1/jobs'));
  releasePoll();
  await completedPoll;
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  await expect(card.getByText('Tags: Saved now', { exact: true })).toBeVisible();
  await card.getByRole('button', { name: 'Edit labels', exact: true }).click();
  await expect(editor.getByRole('textbox', { name: 'Tags', exact: true })).toHaveValue('Saved now');
  await editor.getByRole('textbox', { name: 'Tags', exact: true }).fill('Draft after save');
  await page.waitForResponse(response => response.url().endsWith('/api/v1/movies'));
  await expect(editor.getByRole('textbox', { name: 'Tags', exact: true })).toHaveValue('Draft after save');
});

test('TMDB genre suggestions require selected approval and save, and errors leave manual labels usable', async ({ page }) => {
  const movie = {
    id: movieId, title: 'Linked movie', year: 2001, description: 'Synopsis',
    tmdb_id: 7, metadata_expires_at: Math.floor(Date.now() / 1000) + 60,
    categories: ['Hand-picked'], tags: ['Keep me'],
  };
  let suggestions = 0;
  const saves: Array<{ categories: string[]; tags: string[] }> = [];
  await page.route('**/api/v1/movies', route => route.fulfill({ json: { movies: [movie] } }));
  await page.route(`**/api/v1/movies/${movieId}/labels`, route => route.fulfill({ json: {
    categories: movie.categories, tags: movie.tags,
  } }));
  await page.route('**/api/v1/labels', route => route.fulfill({ json: {
    categories: ['Hand-picked'], tags: ['Keep me'],
  } }));
  await page.route(`**/api/v1/admin/movies/${movieId}/genre-suggestions`, route => {
    expect(route.request().method()).toBe('POST');
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    suggestions++;
    return suggestions === 1
      ? route.fulfill({ status: 503, json: { error: 'TMDB lookup is disabled; enter metadata manually.' } })
      : route.fulfill({ json: { categories: ['Drama', 'Adventure'] } });
  });
  await page.route(`**/api/v1/admin/movies/${movieId}/labels`, async route => {
    expect(route.request().method()).toBe('POST');
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    const request = route.request().postDataJSON();
    expect(request.expected).toEqual({ categories: movie.categories, tags: movie.tags });
    saves.push(request.labels);
    movie.categories = request.labels.categories;
    movie.tags = request.labels.tags;
    await route.fulfill({ json: request.labels });
  });
  await login(page, 'e2e-admin');
  await page.getByRole('button', { name: 'Edit labels', exact: true }).click();
  const editor = page.getByRole('region', { name: 'Edit movie labels', exact: true });
  await editor.getByRole('textbox', { name: 'Categories', exact: true }).fill('Hand-picked\nManual');
  expect(suggestions).toBe(0);
  await editor.getByRole('button', { name: 'Suggest TMDB genres', exact: true }).click();
  await expect(editor.getByRole('alert')).toContainText('TMDB lookup is disabled');
  await expect(editor.getByRole('textbox', { name: 'Categories', exact: true })).toHaveValue('Hand-picked\nManual');
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor).toHaveCount(0);
  expect(saves).toEqual([{ categories: ['Hand-picked', 'Manual'], tags: ['Keep me'] }]);
  await page.getByRole('button', { name: 'Edit labels', exact: true }).click();
  await editor.getByRole('button', { name: 'Suggest TMDB genres', exact: true }).click();
  await expect(editor.getByRole('checkbox', { name: 'Drama', exact: true })).not.toBeChecked();
  await expect(editor.getByRole('checkbox', { name: 'Adventure', exact: true })).not.toBeChecked();
  await editor.getByRole('checkbox', { name: 'Drama', exact: true }).check();
  await page.waitForResponse(response => response.url().endsWith('/api/v1/movies'));
  await expect(editor.getByRole('checkbox', { name: 'Drama', exact: true })).toBeChecked();
  expect(saves).toHaveLength(1);
  await expect(editor.getByRole('textbox', { name: 'Categories', exact: true })).toHaveValue('Hand-picked\nManual');
  await editor.getByRole('button', { name: 'Save labels', exact: true }).click();
  await expect(editor).toHaveCount(0);
  expect(saves[1]).toEqual({ categories: ['Hand-picked', 'Manual', 'Drama'], tags: ['Keep me'] });
});

test('TMDB refresh is explicit and survives library polling without blocking local playback', async ({ page }) => {
  const movie = {
    id: movieId, title: 'source.iso', year: null as number | null, description: '',
    tmdb_id: 7, metadata_expires_at: Math.floor(Date.now() / 1000) - 1,
  };
  let requests = 0;
  let releaseRefresh!: () => void;
  const heldRefresh = new Promise<void>(resolve => { releaseRefresh = resolve; });
  await page.route('**/api/v1/movies', route => route.fulfill({ json: { movies: [movie] } }));
  await page.route(`**/api/v1/admin/movies/${movieId}/metadata/refresh`, async route => {
    expect(route.request().method()).toBe('POST');
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    requests++;
    if (requests === 1) {
      return route.fulfill({ status: 502, json: { error: 'TMDB is unavailable. Try again later or enter metadata manually.' } });
    }
    await heldRefresh;
    movie.title = 'Refreshed title';
    movie.year = 2002;
    movie.description = 'Refreshed synthetic synopsis';
    movie.metadata_expires_at = Math.floor(Date.now() / 1000) + 150 * 86400;
    return route.fulfill({ json: {
      id: 7, title: movie.title, year: movie.year, description: movie.description,
      fetched_at: Math.floor(Date.now() / 1000),
    } });
  });
  await login(page, 'e2e-admin');
  const refresh = page.getByRole('button', { name: 'Refresh TMDB metadata and poster', exact: true });
  await expect(page.getByRole('heading', { name: 'source.iso', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Watch source.iso', exact: true })).toBeEnabled();
  expect(requests).toBe(0);
  await refresh.click();
  await expect(page.getByText('TMDB is unavailable. Try again later or enter metadata manually.')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Watch source.iso', exact: true })).toBeEnabled();
  await refresh.click();
  await expect.poll(() => requests).toBe(2);
  await page.waitForResponse(response => response.url().endsWith('/api/v1/movies') && response.request().method() === 'GET');
  await expect(refresh).toBeDisabled();
  releaseRefresh();
  await expect(page.getByRole('heading', { name: 'Refreshed title', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Watch Refreshed title', exact: true })).toBeEnabled();
  expect(requests).toBe(2);
});

test('movie posters use private artwork with generic fallback, explicit refresh and matching player icons', async ({ page }) => {
  const movie = {
    id: movieId, title, year: null, description: 'Poster browser fixture',
    tmdb_id: 7, metadata_expires_at: Math.floor(Date.now() / 1000) + 3600,
    poster_expires_at: (Math.floor(Date.now() / 1000) + 3600) as number | null,
    categories: [], tags: [],
  };
  let refreshed = false;
  let refreshes = 0;
  const imageRequests: string[] = [];
  const externalRequests: string[] = [];
  page.on('request', request => {
    if (new URL(request.url()).origin !== baseURL) externalRequests.push(request.url());
  });
  await page.route('**/api/v1/movies', route => route.fulfill({ json: { movies: [movie] } }));
  await page.route(`**/api/v1/movies/${movieId}/poster?*`, route => {
    imageRequests.push(route.request().url());
    return refreshed
      ? route.fulfill({
        contentType: 'image/png',
        body: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a23sAAAAASUVORK5CYII=', 'base64'),
        headers: { 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' },
      })
      : route.fulfill({ status: 404, body: '' });
  });
  await page.route(`**/api/v1/admin/movies/${movieId}/metadata/refresh`, route => {
    expect(route.request().method()).toBe('POST');
    expect(route.request().headers()['x-csrf-token']).toBeTruthy();
    refreshes++;
    refreshed = true;
    return route.fulfill({ json: {
      id: 7, title, year: null, description: movie.description, fetched_at: Math.floor(Date.now() / 1000),
    } });
  });
  await login(page, 'e2e-admin');
  const card = page.locator('article.movie').filter({ has: page.getByRole('heading', { name: title, exact: true }) });
  await expect(card.getByRole('img', { name: 'No poster available', exact: true })).toBeVisible();
  await expect.poll(() => imageRequests.length).toBe(1);
  expect(refreshes).toBe(0);
  await card.getByRole('button', { name: 'Refresh TMDB metadata and poster', exact: true }).click();
  const poster = card.getByRole('img', { name: `Poster for ${title}`, exact: true });
  await expect(poster).toBeVisible();
  await expect.poll(() => poster.evaluate(image => (image as HTMLImageElement).naturalWidth)).toBe(1);
  await expect(card.getByRole('img', { name: 'No poster available', exact: true })).toHaveCount(0);
  const source = await poster.getAttribute('src');
  await card.getByRole('button', { name: `Watch ${title}`, exact: true }).click();
  const pane = page.getByRole('region', { name: `Watch ${title}`, exact: true });
  const icon = pane.getByRole('img', { name: `Poster for ${title}`, exact: true });
  await expect(icon).toHaveAttribute('src', source!);
  await expect(pane.locator('.movie-icon')).toHaveCSS('width', '48px');
  await expect(page.getByTestId('movie-player')).toBeVisible();
  await pane.getByRole('button', { name: 'Back to library', exact: true }).click();
  movie.poster_expires_at = null;
  await expect(card.getByRole('img', { name: 'No poster available', exact: true })).toBeVisible();
  const cachedReads = imageRequests.length;
  movie.poster_expires_at = Math.floor(Date.now() / 1000) + 3600;
  await expect(card.getByRole('img', { name: `Poster for ${title}`, exact: true })).toBeVisible();
  await expect.poll(() => imageRequests.length).toBeGreaterThan(cachedReads);
  expect(refreshes).toBe(1);
  expect(externalRequests).toEqual([]);
});

test('live media progress renders measurable, indeterminate, queued, review, failed and terminal stages honestly', async ({ page }) => {
  test.setTimeout(120_000);
  const now = () => Math.floor(Date.now() / 1000);
  const scan = {
    id: randomUUID(), name: 'Progress fixture.iso', state: 'queued',
    created_at: now(), attempt_id: null, report: null, error: null,
    progress: null as null | Record<string, unknown>,
  };
  const conversion = {
    id: randomUUID(), probe_job_id: randomUUID(), state: 'queued',
    created_at: now(), attempt_id: null, video_treatment: 'preserve',
    phase: null, report: null, error: null, movie_id: null, movie_sha256: null,
    progress: {
      workflow: 'conversion', stage: 2, phase: 'Encoding movie',
      elapsed_seconds: 10, completed_units: 20, total_units: 80 as number | null,
      unit: 'media seconds', updated_at: now(),
      measured_elapsed_seconds: 10, stage_portion: null as number[] | null,
    },
  };
  await page.route('**/api/v1/admin/imports?*', route => {
    const history = new URL(route.request().url()).searchParams.get('history') === 'true';
    const entries = [
      { item: { kind: 'scan', job: scan }, can_archive: scan.state === 'failed', history: false },
      { item: { kind: 'conversion', job: conversion }, can_archive: ['failed', 'published'].includes(conversion.state),
        history: conversion.state === 'published' },
    ];
    return route.fulfill({ headers: { date: new Date().toUTCString() },
      json: { entries: entries.filter(entry => entry.history === history),
      total: entries.filter(entry => entry.history === history).length,
      history_count: entries.filter(entry => entry.history).length } });
  });
  await login(page, 'e2e-admin');
  const scanCard = page.locator(`[data-testid="scan-job"][data-job-id="${scan.id}"]`);
  const convertedCard = page.locator(`[data-testid="conversion-job"][data-job-id="${conversion.id}"]`);
  await expect(scanCard.getByRole('progressbar')).toHaveCount(3);
  await expect(convertedCard.getByRole('progressbar')).toHaveCount(7);
  await expect(convertedCard.getByRole('progressbar', { name: 'Encode', exact: true })).toHaveAttribute('aria-valuenow', '0');
  await expect(convertedCard.locator('.phase-progress')).toHaveText('Queued — waiting for worker.');

  conversion.state = 'converting';
  conversion.progress.updated_at = now();
  await expect(convertedCard.getByRole('progressbar', { name: 'Revalidate input', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(convertedCard.getByRole('progressbar', { name: 'Decode source', exact: true })).toHaveAttribute('aria-valuenow', '100');
  const encode = convertedCard.getByRole('progressbar', { name: 'Encode', exact: true });
  await expect(encode).toHaveAttribute('aria-valuenow', '25');
  await expect(convertedCard.locator('.phase-progress')).toContainText('elapsed 10s');
  await expect(convertedCard.locator('.phase-progress')).toContainText('2.0 media seconds/s · estimated 30s remaining in this phase');
  await expect(convertedCard.getByText('Progress by stage, not time remaining', { exact: true })).toBeVisible();
  await page.waitForTimeout(2200);
  await expect(convertedCard.locator('.phase-progress')).toContainText(/estimated 2[678]s remaining/);
  // Heartbeats advance observation age, not the measurement used for the estimate.
  conversion.progress.elapsed_seconds = 22;
  conversion.progress.updated_at = now();
  await expect(convertedCard.locator('.phase-progress')).toContainText(/estimated 1[678]s remaining/);
  await expect(encode).toHaveAttribute('aria-valuenow', '25');
  conversion.progress.elapsed_seconds = 45;
  await expect(convertedCard.locator('.phase-progress')).toContainText('waiting for updated estimate');
  await expect(encode).toHaveAttribute('aria-valuenow', '25');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('Workflow complete');
  conversion.progress.completed_units = 40;
  conversion.progress.measured_elapsed_seconds = 45;
  await expect(encode).toHaveAttribute('aria-valuenow', '50');
  await expect(convertedCard.locator('.phase-progress')).toContainText(/estimated 4[345]s remaining/);

  conversion.progress.completed_units = 80;
  await expect(encode).toHaveAttribute('aria-valuenow', '99');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('remaining');
  conversion.progress.total_units = null;
  await expect(encode).not.toHaveAttribute('aria-valuenow');
  await expect(convertedCard.locator('.phase-progress')).toContainText('indeterminate');

  conversion.progress.stage = 0;
  conversion.progress.phase = 'Revalidating retained input checksum';
  conversion.progress.completed_units = 100;
  conversion.progress.total_units = 100;
  conversion.progress.stage_portion = [0, 0.5];
  const input = convertedCard.getByRole('progressbar', { name: 'Revalidate input', exact: true });
  await expect(input).toHaveAttribute('aria-valuenow', '50');
  conversion.progress.phase = 'Reinspecting checksum-bound program';
  conversion.progress.total_units = null;
  conversion.progress.stage_portion = [0.5, 1];
  await expect(convertedCard.locator('.phase-progress')).toContainText('Reinspecting checksum-bound program · indeterminate');
  await expect(input).toHaveAttribute('aria-valuenow', '50');
  conversion.state = 'publishing';
  conversion.progress.stage = 5;
  const publication = convertedCard.getByRole('progressbar', { name: 'Copy & verify publication', exact: true });
  for (const portions of [3, 5]) {
    for (let step = 0; step < portions; step++) {
      conversion.progress.phase = `Publication check ${step + 1} of ${portions}`;
      conversion.progress.stage_portion = [step / portions, (step + 1) / portions];
      conversion.progress.total_units = null;
      await expect(publication).toHaveAttribute('aria-valuenow', (100 * step / portions).toFixed(0));
      conversion.progress.total_units = 100;
      conversion.progress.completed_units = 50;
      await expect(publication).toHaveAttribute('aria-valuenow', (100 * (step + 0.5) / portions).toFixed(0));
    }
  }
  await expect(convertedCard.getByRole('progressbar', { name: 'Register', exact: true })).toHaveAttribute('aria-valuenow', '0');
  conversion.state = 'converting';
  conversion.progress.stage = 2;
  conversion.progress.phase = 'Encoding movie';
  conversion.progress.stage_portion = null;

  scan.state = 'scanning';
  scan.progress = {
    workflow: 'scan', stage: 2, phase: 'Discovering DVD titles',
    elapsed_seconds: 3, completed_units: null, total_units: null, unit: '', updated_at: now(),
  };
  await expect(scanCard.getByRole('progressbar', { name: 'Copy input', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(scanCard.getByRole('progressbar', { name: 'Verify input', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(scanCard.getByRole('progressbar', { name: 'Discover DVD', exact: true })).not.toHaveAttribute('aria-valuenow');
  await expect(scanCard.locator('.phase-progress')).not.toContainText('remaining');

  conversion.progress.completed_units = 20;
  conversion.progress.total_units = 80;
  conversion.progress.measured_elapsed_seconds = 10;
  conversion.progress.elapsed_seconds = 45;
  conversion.progress.updated_at = now() - 60;
  await expect(convertedCard.locator('.phase-progress')).toContainText('last recorded; not live');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('remaining');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('waiting for updated estimate');
  conversion.state = 'failed';
  conversion.progress.updated_at = now();
  await expandImport(page, conversion.id);
  await expect(convertedCard.getByText('Failed — ISO, original tracks, reports and logs retained', { exact: true })).toBeVisible();
  await expect(encode).toHaveAttribute('aria-valuenow', '25');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('remaining');

  conversion.state = 'needs_review';
  await expect(convertedCard.locator('.phase-progress')).toHaveText('Awaiting administrator review — not published.');
  await expect(convertedCard.getByRole('progressbar', { name: 'Validate output', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(convertedCard.getByRole('progressbar', { name: 'Review', exact: true })).toHaveAttribute('aria-valuenow', '0');
  await expect(convertedCard.getByRole('progressbar', { name: 'Register', exact: true })).toHaveAttribute('aria-valuenow', '0');
  await expect(convertedCard.getByRole('button', { name: 'Publish to local library', exact: true })).toHaveCount(0);
  conversion.state = 'queued_publication';
  await expect(convertedCard.getByRole('progressbar', { name: 'Review', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(convertedCard.getByRole('progressbar', { name: 'Copy & verify publication', exact: true })).toHaveAttribute('aria-valuenow', '0');
  conversion.state = 'published';
  await expandImport(page, conversion.id, true);
  await expect(convertedCard).toHaveCount(1);
  await expect(page.locator(`[data-testid="attention-row"][data-job-id="${conversion.id}"]`)).toHaveCount(0);
  await expect(convertedCard.getByRole('progressbar', { name: 'Register', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(convertedCard.locator('.phase-progress')).toHaveText('Workflow complete.');
  scan.state = 'needs_review';
  await expandImport(page, scan.id);
  await expect(scanCard.getByRole('progressbar', { name: 'Discover DVD', exact: true })).toHaveAttribute('aria-valuenow', '100');
});

test('media progress heartbeat liveness survives clock skew without refreshing measured ETA', async ({ page }) => {
  const started = Date.now();
  const now = () => Math.floor(Date.now() / 1000);
  const job = (updatedAt: number) => ({
    id: randomUUID(), probe_job_id: randomUUID(), state: 'converting', created_at: now(),
    attempt_id: null, video_treatment: 'preserve', phase: 'encoding', report: null,
    error: null, movie_id: null, movie_sha256: null,
    progress: {
      workflow: 'conversion', stage: 2, phase: 'Encoding movie', elapsed_seconds: 10,
      completed_units: 20, total_units: 80, unit: 'media seconds', updated_at: updatedAt,
      measured_elapsed_seconds: 10,
    },
  });
  const live = job(now());
  const stale = job(now() - 3600);
  let heartbeats = true;
  let estimateExpired = false;
  await page.clock.setFixedTime(new Date('2042-06-01T12:00:00Z'));
  await page.route('**/api/v1/admin/imports?*', route => {
    if (heartbeats) {
      live.progress.updated_at = now();
      live.progress.elapsed_seconds = estimateExpired ? 45 : 10 + (Date.now() - started) / 1000;
    }
    const history = new URL(route.request().url()).searchParams.get('history') === 'true';
    const entries = history ? [] : [live, stale].map(job => ({
      item: { kind: 'conversion', job }, can_archive: job.state === 'failed',
    }));
    return route.fulfill({
      headers: { date: new Date().toUTCString() },
      json: { entries, total: entries.length, history_count: 0 },
    });
  });
  await login(page, 'e2e-admin');
  const liveCard = page.locator(`[data-testid="conversion-job"][data-job-id="${live.id}"]`);
  const staleCard = page.locator(`[data-testid="conversion-job"][data-job-id="${stale.id}"]`);
  const livePhase = liveCard.locator('.phase-progress');
  const stalePhase = staleCard.locator('.phase-progress');
  const encode = liveCard.getByRole('progressbar', { name: 'Encode', exact: true });
  await expect(stalePhase).toContainText('last recorded; not live');
  await expect(stalePhase).not.toContainText(/remaining|waiting for updated estimate/);
  await expect(staleCard.getByRole('progressbar', { name: 'Encode', exact: true })).toHaveAttribute('aria-valuenow', '25');
  await expect(livePhase).toContainText(/estimated (30|2[789])s remaining/);
  await page.waitForTimeout(11_000);
  await expect(encode).toHaveAttribute('aria-valuenow', '25');
  await expect(livePhase).toContainText(/estimated 1[56789]s remaining/);
  await expect(livePhase).not.toContainText('not live');
  heartbeats = false;
  await expect(livePhase).toContainText('last recorded; not live', { timeout: 15_000 });
  await expect(livePhase).not.toContainText(/remaining|waiting for updated estimate/);
  await expect(encode).toHaveAttribute('aria-valuenow', '25');
  heartbeats = true;
  estimateExpired = true;
  await expect(livePhase).toContainText('waiting for updated estimate');
  await expect(livePhase).not.toContainText('not live');
  live.state = 'failed';
  await expect(livePhase).toContainText('last recorded; not live');
  await expect(livePhase).not.toContainText(/remaining|waiting for updated estimate/);
});

test('midpoint preview progress polls measured and indeterminate phases without implying readiness', async ({ page }) => {
  const now = () => Math.floor(Date.now() / 1000);
  const probe = {
    id: randomUUID(), name: 'Preview progress.iso', title: 1, angle: 1, state: 'needs_review',
    created_at: now(), attempt_id: randomUUID(), error: null,
    report: { sha256: 'a'.repeat(64), inspection: { warnings: [], metadata: {
      format: { duration: '120' }, streams: [
        { codec_type: 'video', codec_name: 'mpeg2video', width: 720, height: 480 },
      ],
    } } },
  };
  const job = {
    id: randomUUID(), probe_job_id: probe.id, state: 'queued', created_at: now(),
    attempt_id: null, report: null, error: null as string | null,
    progress: null as null | Record<string, unknown>,
  };
  await page.route('**/api/v1/admin/imports?*', route => {
    const history = new URL(route.request().url()).searchParams.get('history') === 'true';
    return route.fulfill({ json: { entries: history ? [] : [
      { item: { kind: 'probe', job: probe }, can_archive: false },
    ], total: history ? 0 : 1, history_count: 0 } });
  });
  let submitted = false;
  await page.route(`**/api/v1/admin/probes/${probe.id}/comparison`, route => {
    if (route.request().method() === 'POST') {
      submitted = true;
      return route.fulfill({ status: 202, json: job });
    }
    return route.fulfill({ headers: { date: new Date().toUTCString() }, json: submitted ? job : null });
  });
  await login(page, 'e2e-admin');
  await expandImport(page, probe.id);
  const preview = page.getByTestId('comparison-preview');
  const generate = preview.getByRole('button', { name: 'Generate Preserve vs Yadif preview', exact: true });
  const refresh = preview.getByRole('button', { name: 'Refresh comparison status', exact: true });
  await expect(generate).toBeEnabled();
  const generateBox = (await generate.boundingBox())!;
  const refreshBox = (await refresh.boundingBox())!;
  const verticalGap = refreshBox.y - (generateBox.y + generateBox.height);
  const horizontalGap = refreshBox.x - (generateBox.x + generateBox.width);
  expect(Math.max(verticalGap, horizontalGap)).toBeGreaterThan(4);
  await generate.click();
  await expect(preview.getByRole('progressbar')).toHaveCount(8);
  await expect(preview.locator('.phase-progress')).toHaveText('Queued — waiting for worker.');
  await expect(generate).toBeDisabled();
  await expect(preview.locator('video')).toHaveCount(0);
  job.state = 'running';
  job.progress = {
    workflow: 'comparison', stage: 0, phase: 'Revalidating retained input checksum',
    elapsed_seconds: 10, completed_units: 25, total_units: 100, unit: 'bytes', updated_at: now(),
  };
  const revalidate = preview.getByRole('progressbar', { name: 'Revalidate input', exact: true });
  await expect(revalidate).toHaveAttribute('aria-valuenow', '25');
  await expect(preview.locator('.phase-progress')).toContainText('2.5 bytes/s · estimated 30s remaining');
  for (const [stage, name] of [
    [1, 'Extract shared sample'], [2, 'Validate shared sample'], [3, 'Encode Preserve'],
    [4, 'Validate Preserve'], [5, 'Encode Yadif'], [6, 'Validate Yadif'],
  ] as const) {
    job.progress = { ...job.progress, stage, phase: name, completed_units: 5, total_units: 10,
      unit: 'media seconds', updated_at: now() };
    const bar = preview.getByRole('progressbar', { name, exact: true });
    await expect(bar).toHaveAttribute('aria-valuenow', '50');
    await expect(preview.locator('.phase-progress')).toContainText('0.5 media seconds/s');
    await expect(preview.locator('video')).toHaveCount(0);
  }
  job.progress = { ...job.progress, phase: 'Reading decode-yadif metadata', completed_units: null,
    total_units: null, updated_at: now() };
  const validate = preview.getByRole('progressbar', { name: 'Validate Yadif', exact: true });
  await expect(validate).not.toHaveAttribute('aria-valuenow');
  await expect(preview.locator('.phase-progress')).toContainText('indeterminate');
  await expect(preview.locator('.phase-progress')).not.toContainText('remaining');
  job.progress = { ...job.progress, completed_units: 5, total_units: 10, updated_at: now() - 60,
    measured_elapsed_seconds: 10, elapsed_seconds: 25 };
  await expect(preview.locator('.phase-progress')).toContainText('last recorded; not live');
  await expect(preview.locator('.phase-progress')).not.toContainText('remaining');
  await expect(preview.locator('.phase-progress')).not.toContainText('waiting for updated estimate');
  job.state = 'failed';
  job.error = 'Injected validation failure; artifacts retained.';
  job.progress.updated_at = now();
  await expect(preview.getByRole('alert')).toContainText('Injected validation failure');
  await expect(validate).toHaveAttribute('aria-valuenow', '50');
  await expect(preview.locator('.phase-progress')).toContainText('last recorded; not live');
  await expect(preview.locator('.phase-progress')).not.toContainText('remaining');
  await expect(preview.locator('video')).toHaveCount(0);
  await expect(generate).toBeEnabled();
  job.state = 'running';
  job.error = null;
  job.progress = { ...job.progress, stage: 7, phase: 'Finalizing comparison report',
    completed_units: null, total_units: null, updated_at: now() };
  await refresh.click();
  const finalize = preview.getByRole('progressbar', { name: 'Finalize comparison', exact: true });
  await expect(finalize).not.toHaveAttribute('aria-valuenow');
  job.state = 'succeeded';
  await expect(preview.locator('.phase-progress')).toHaveText('Workflow complete.');
  await expect(finalize).toHaveAttribute('aria-valuenow', '100');
  await expect(preview.locator('video')).toHaveCount(2);
  await expect(preview).not.toContainText(/NaN|Infinity/);
});

for (const profile of [
  {
    timezoneId: 'America/Los_Angeles', locale: 'en-US',
    times: [
      /Mar 7, 2026, 11:30:00 PM GMT-08:00/,
      /Mar 8, 2026, 1:30:00 AM GMT-08:00/,
      /Mar 8, 2026, 3:30:00 AM GMT-07:00/,
    ],
  },
  {
    timezoneId: 'Asia/Tokyo', locale: 'en-GB',
    times: [
      /8 Mar 2026, 16:30:00 GMT\+09:00/,
      /8 Mar 2026, 18:30:00 GMT\+09:00/,
      /8 Mar 2026, 19:30:00 GMT\+09:00/,
    ],
  },
]) {
  test(`local timestamps follow browser timezone and locale (${profile.timezoneId})`, async ({ browser }) => {
    const context = await browser.newContext({ timezoneId: profile.timezoneId, locale: profile.locale });
    const page = await context.newPage();
    const instants = ['2026-03-08T07:30:00.000Z', '2026-03-08T09:30:00.000Z', '2026-03-08T10:30:00.000Z'];
    const seconds = instants.map(instant => Date.parse(instant) / 1000);
    const diagnostics = seconds.map((timestamp, index) => ({
      id: randomUUID(), state: index === 0 ? 'queued' : 'failed', created_at: timestamp - (index ? 10 : 0),
      finished_at: index === 0 ? null : timestamp, error: null, report: null,
    }));
    diagnostics.push({ id: randomUUID(), state: 'queued', created_at: 8_640_000_000_001,
      finished_at: null, error: null, report: null });
    const probe = {
      id: randomUUID(), name: 'Local dates.iso', title: 1, angle: 1, state: 'failed',
      created_at: seconds[2], attempt_id: null, error: null, progress: null,
      report: { fetched_at: seconds[2], inspection: { metadata: { format: { duration: '73.5' } } } },
    };
    const entries = [
      { item: { kind: 'scan', job: {
        id: randomUUID(), name: 'Local dates.iso', state: 'failed', created_at: seconds[1],
        attempt_id: null, report: null, error: null, progress: null,
      } }, can_archive: true, history: false },
      { item: { kind: 'probe', job: probe }, can_archive: true, history: false },
      { item: { kind: 'conversion', job: {
        id: randomUUID(), probe_job_id: probe.id, state: 'published', created_at: seconds[0],
        attempt_id: null, video_treatment: 'preserve', phase: null, report: null,
        error: null, movie_id: null, movie_sha256: null, progress: null,
      } }, can_archive: true, history: true },
    ];
    try {
      await page.route('**/api/v1/jobs', route => route.fulfill({ json: diagnostics }));
      await page.route('**/api/v1/movies', route => route.fulfill({ json: { movies: [{
        id: movieId, title, year: null, description: '', tmdb_id: 7, metadata_expires_at: seconds[0],
      }] } }));
      await page.route('**/api/v1/admin/imports?*', route => {
        const history = new URL(route.request().url()).searchParams.get('history') === 'true';
        const selected = entries.filter(entry => entry.history === history);
        return route.fulfill({ json: { entries: selected, total: selected.length, history_count: 1 } });
      });
      const jobsResponse = page.waitForResponse(response => response.url().endsWith('/api/v1/jobs'));
      await login(page, 'e2e-admin');
      expect(await (await jobsResponse).json()).toEqual(diagnostics);
      const latest = page.getByTestId('latest-readiness');
      await expect(latest).toContainText('Requested:');
      await expect(latest.locator('time')).toHaveText(profile.times[0]);
      await expect(latest.locator('time')).toHaveAttribute('datetime', instants[0]);
      const previous = page.getByTestId('previous-checks');
      await previous.locator(':scope > summary').click();
      for (const index of [1, 2]) {
        const check = previous.getByTestId('previous-readiness').nth(index - 1);
        await expect(check).toContainText('Last checked:');
        await expect(check.locator('time')).toHaveText(profile.times[index]);
        await expect(check.locator('time')).toHaveAttribute('datetime', instants[index]);
      }
      const invalid = previous.getByTestId('previous-readiness').last().locator('time');
      await expect(invalid).toHaveText('Unknown time');
      await expect(invalid).not.toHaveAttribute('datetime');
      await expect(page.getByText(/^TMDB metadata expires:/)).toContainText(profile.times[0]);
      for (const [index, entry] of entries.slice(0, 2).entries()) {
        await expect(page.locator(`[data-testid="attention-row"][data-job-id="${entry.item.job.id}"]`)
          .locator('summary').first()).toContainText(profile.times[index + 1]);
      }
      const history = page.getByTestId('import-history');
      await history.locator(':scope > summary').click();
      await expect(history.getByTestId('history-row').locator('summary').first()).toContainText(profile.times[0]);
      await expandImport(page, probe.id);
      const probeCard = page.locator(`[data-testid="probe-job"][data-job-id="${probe.id}"]`);
      await expect(probeCard).toContainText('About 1 min (73.5 s)');
      await probeCard.getByText('Inspection details (JSON)', { exact: true }).click();
      expect(JSON.parse(await probeCard.locator('pre').innerText())).toEqual(probe.report);
    } finally {
      await context.close();
    }
  });
}

test('compact latest readiness, attention disclosures and demand-loaded older history', async ({ page }) => {
  const timestamp = Math.floor(Date.now() / 1000);
  const readyReport = { tools: ['ffmpeg', 'ffprobe', 'dvdvideo', 'DVD discovery'].map(name => ({
    name, available: true, detail: 'Synthetic capability available',
  })), dvdvideo_available: true };
  const diagnosticJobs: Array<{
    id: string; state: string; created_at: number; finished_at: number | null;
    error: string | null; report: typeof readyReport | null;
  }> = [
    { id: randomUUID(), state: 'queued', created_at: timestamp - 5, finished_at: null, error: null, report: null },
    { id: randomUUID(), state: 'failed', created_at: timestamp - 65, finished_at: timestamp - 60, error: 'Previous synthetic failure', report: null },
  ];
  await page.route('**/api/v1/jobs', route => route.fulfill({ json: diagnosticJobs }));
  await page.route('**/api/v1/jobs/diagnostics', route => {
    const job = { id: randomUUID(), state: 'failed', created_at: timestamp, finished_at: timestamp + 1,
      error: 'Newest synthetic failure', report: null };
    diagnosticJobs.unshift(job);
    return route.fulfill({ status: 202, json: job });
  });
  const scan = (state: string, name: string, error: string | null = null) => ({
    id: randomUUID(), state, name, created_at: timestamp, attempt_id: null, report: null, error,
  });
  const review = scan('needs_review', 'Review fixture.iso');
  const failed = scan('failed', 'Failed fixture.iso', 'Actionable failure remains visible');
  const active = { id: randomUUID(), probe_job_id: randomUUID(), state: 'converting', created_at: timestamp,
    attempt_id: null, video_treatment: 'preserve', phase: null, report: null, error: null,
    movie_id: null, movie_sha256: null, progress: null };
  const historyEntries = Array.from({ length: 12 }, (_, index) => ({
    item: { kind: 'scan', job: scan('failed', `Old ${index}.iso`) }, can_archive: true,
  }));
  const historyCursors: Array<string | null> = [];
  await page.route('**/api/v1/admin/imports?*', route => {
    const query = new URL(route.request().url()).searchParams;
    const history = query.get('history') === 'true';
    const before = query.get('before');
    const offset = before ? historyEntries.findIndex(entry => `scan:${entry.item.job.id}` === before) + 1 : 0;
    if (history) historyCursors.push(before);
    const entries = history ? historyEntries.slice(offset, offset + Number(query.get('limit'))) : [
      { item: { kind: 'scan', job: review }, can_archive: false },
      { item: { kind: 'scan', job: failed }, can_archive: true },
      { item: { kind: 'conversion', job: active }, can_archive: false },
    ];
    const next = history && offset + entries.length < historyEntries.length
      ? `scan:${historyEntries[offset + entries.length - 1].item.job.id}` : null;
    return route.fulfill({ json: { entries, total: history ? 12 : 3, history_count: 12, next } });
  });
  await login(page, 'e2e-admin');
  const latest = page.getByTestId('latest-readiness');
  await expect(latest).toHaveCount(1);
  await expect(latest).toContainText('Queued — waiting for worker');
  await expect(latest).toContainText('Requested:');
  await expect(latest.getByText('Synthetic capability available').first()).not.toBeVisible();
  await latest.getByText('Individual checks', { exact: true }).click();
  diagnosticJobs[0].state = 'succeeded';
  diagnosticJobs[0].finished_at = timestamp;
  diagnosticJobs[0].report = readyReport;
  await expect(latest).toContainText('Required media capabilities available');
  await expect(latest).toContainText('Last checked:');
  await expect(latest.locator('time')).toHaveAttribute('datetime', new Date(timestamp * 1000).toISOString());
  await expect(latest.locator('details')).toHaveAttribute('open');
  await expect(latest.getByText('Synthetic capability available').first()).toBeVisible();
  const previous = page.getByTestId('previous-checks');
  await expect(previous).not.toHaveAttribute('open');
  await expect(previous.getByText('Previous synthetic failure', { exact: true })).not.toBeVisible();
  await previous.locator(':scope > summary').click();
  await expect(previous.getByText('Previous synthetic failure', { exact: true })).toBeVisible();
  await previous.locator(':scope > summary').click();
  await page.getByRole('button', { name: 'Check again', exact: true }).click();
  await expect(latest).toContainText('Newest synthetic failure');
  await expect(latest).toContainText('Media capabilities need attention');
  await expect(latest).toHaveCount(1);
  const failedRow = page.locator(`[data-testid="attention-row"][data-job-id="${failed.id}"]`);
  await expect(failedRow.locator(':scope > details')).not.toHaveAttribute('open');
  await expect(failedRow.locator(':scope > .error')).toHaveText('Actionable failure remains visible');
  await expect(failedRow.getByRole('button', { name: 'Archive to History', exact: true })).toBeVisible();
  for (const id of [review.id, active.id]) {
    await expect(page.locator(`[data-testid="attention-row"][data-job-id="${id}"]`)
      .getByRole('button', { name: 'Archive to History', exact: true })).toHaveCount(0);
  }
  await expect(page.locator(`[data-testid="conversion-job"][data-job-id="${active.id}"]`).getByRole('progressbar')).toHaveCount(7);
  await expandImport(page, review.id);
  await expect(page.locator(`[data-testid="scan-job"][data-job-id="${review.id}"]`)).toBeVisible();
  const historyPanel = page.getByTestId('import-history');
  await expect(historyPanel.locator(':scope > summary')).toHaveText('History (12)');
  await expect(historyPanel).not.toHaveAttribute('open');
  expect(historyCursors).toEqual([]);
  await historyPanel.locator(':scope > summary').click();
  await expect(historyPanel.getByTestId('history-row')).toHaveCount(10);
  await expect(historyPanel.locator('.import-details[open]')).toHaveCount(0);
  expect(historyCursors).toEqual([null]);
  await historyPanel.getByRole('button', { name: 'Load older', exact: true }).click();
  await expect(historyPanel.getByTestId('history-row')).toHaveCount(12);
  expect(historyCursors).toEqual([null, `scan:${historyEntries[9].item.job.id}`]);
  await expect(historyPanel.getByRole('button', { name: 'Load older', exact: true })).toHaveCount(0);
  await historyPanel.locator(':scope > summary').click();
  await expect(historyPanel.getByTestId('history-row').first()).not.toBeVisible();
});

test('archive and restore persist across reload without hiding another administrator attention', async ({ page, browser }) => {
  await login(page, 'e2e-admin');
  const session = await (await page.request.get(`${baseURL}/api/v1/auth/me`)).json();
  const incoming = await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json();
  const file = incoming.files.find(file => file.name === 'browser-invalid.iso');
  const queued = await page.request.post(`${baseURL}/api/v1/admin/scans`, {
    headers: { Origin: baseURL, 'x-csrf-token': session.csrf_token },
    data: { name: file.name, identity: file.identity, copy_complete: true },
  });
  expect(queued.status()).toBe(202);
  const scan = await queued.json();
  await expect.poll(async () => {
    const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
    return jobs.find(job => job.id === scan.id)?.state;
  }).toBe('failed');
  const row = page.locator(`[data-testid="attention-row"][data-job-id="${scan.id}"]`);
  await expect(row.locator(':scope > .error')).toBeVisible();
  const retainedError = await row.locator(':scope > .error').textContent();
  await expect(row.locator(':scope > details')).not.toHaveAttribute('open');
  await row.getByRole('button', { name: 'Archive to History', exact: true }).click();
  await expect(row).toHaveCount(0);
  await page.reload();
  await expect(page.getByTestId('import-history')).toBeVisible();
  await expect(row).toHaveCount(0);
  await expect(page.getByTestId('import-history')).not.toHaveAttribute('open');
  const archived = await expandImport(page, scan.id, true);
  await expect(archived.locator(':scope > .error')).toHaveText(retainedError!);
  const isolated = await browser.newContext();
  try {
    const otherPage = await isolated.newPage();
    await login(otherPage, 'e2e-other-admin');
    const otherRow = otherPage.locator(`[data-testid="attention-row"][data-job-id="${scan.id}"]`);
    await expect(otherRow.locator(':scope > .error')).toHaveText(retainedError!);
    await expect(otherRow.getByRole('button', { name: 'Archive to History', exact: true })).toBeVisible();
  } finally {
    await isolated.close();
  }
  await archived.getByRole('button', { name: 'Restore to attention', exact: true }).click();
  await expect(row.locator(':scope > .error')).toHaveText(retainedError!);
  await page.reload();
  await expect(row.locator(':scope > .error')).toHaveText(retainedError!);
  const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
  const retained = jobs.find(job => job.id === scan.id);
  expect(retained.state).toBe('failed');
  expect(retained.attempt_id).toBeTruthy();
  expect(existsSync(join(artifactsRoot!, 'scratch', retained.attempt_id, 'input.iso'))).toBe(true);
});

test('real DVD scan, selected inspection, conversion, publication and playback preserve inputs', async ({ page }) => {
  test.setTimeout(120_000);
  await login(page, 'e2e-admin');
  const original = (await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json()).files.find(file => file.name === 'browser-valid.iso');
  const originalHash = command('sha256sum', [dvdIncomingFile]).split(/\s+/, 1)[0];
  await page.getByRole('button', { name: 'Refresh incoming files', exact: true }).click();
  const incomingSelect = page.getByRole('combobox', { name: 'Completed ISO copy', exact: true });
  const selectedValue = await incomingSelect.locator('option').filter({ hasText: 'browser-valid.iso' }).getAttribute('value');
  expect(selectedValue).toBeTruthy();
  await incomingSelect.selectOption(selectedValue!);
  await page.getByRole('checkbox', { name: /I confirm this copy is finished/ }).check();
  const scanResponse = page.waitForResponse(response => response.url().endsWith('/api/v1/admin/scans') && response.request().method() === 'POST');
  await page.getByRole('button', { name: 'Scan DVD', exact: true }).click();
  const scan = await (await scanResponse).json();
  const scanCard = page.locator(`[data-testid="scan-job"][data-job-id="${scan.id}"]`);
  const titleSelect = scanCard.getByRole('combobox', { name: 'DVD title', exact: true });
  await expandImport(page, scan.id);
  await expect(titleSelect.locator('option')).toHaveCount(3, { timeout: 30_000 });
  const { jobs: scans } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
  const completedScan = scans.find(job => job.id === scan.id);
  expect(completedScan.progress.workflow).toBe('scan');
  expect(completedScan.progress.stage).toBe(2);
  await expect(scanCard.getByRole('progressbar', { name: 'Discover DVD', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await titleSelect.selectOption('2');
  await page.waitForResponse(response => response.url().includes('/api/v1/admin/imports?')
    && !new URL(response.url()).searchParams.has('history'));
  await expect(titleSelect).toHaveValue('2');
  await expect(page.locator(`[data-testid="attention-row"][data-job-id="${scan.id}"] > details.import-details`)).toHaveAttribute('open');
  await expect(scanCard.getByRole('combobox', { name: 'DVD angle', exact: true })).toHaveCount(0);

  const probeResponse = page.waitForResponse(response => response.url().endsWith(`/admin/scans/${scan.id}/inspect`) && response.request().method() === 'POST');
  await scanCard.getByRole('button', { name: 'Inspect selected title', exact: true }).click();
  const probe = await (await probeResponse).json();
  expect(probe.attempt_id).toBe(completedScan.attempt_id);
  const probeCard = page.locator(`[data-testid="probe-job"][data-job-id="${probe.id}"]`);
  const convert = probeCard.getByRole('button', { name: 'Convert reviewed title', exact: true });
  await expandImport(page, probe.id);
  await expect(convert).toBeVisible({ timeout: 30_000 });
  const shortComparison = await compareInspectedProgram(page, probe.id);
  expect(shortComparison.report.requested_start_seconds).toBe(0);
  expect(shortComparison.report.requested_duration_seconds).toBeLessThan(4);
  await probeCard.getByRole('combobox', { name: 'Video treatment', exact: true }).selectOption('preserve');
  await probeCard.getByRole('checkbox', { name: /I reviewed this inspection and warnings/ }).check();
  await page.waitForResponse(response => response.url().includes('/api/v1/admin/imports?')
    && !new URL(response.url()).searchParams.has('history'));
  await expect(probeCard.getByRole('combobox', { name: 'Video treatment', exact: true })).toHaveValue('preserve');
  await expect(probeCard.getByRole('checkbox', { name: /I reviewed this inspection and warnings/ })).toBeChecked();
  const conversionResponse = page.waitForResponse(response => response.url().endsWith('/api/v1/admin/conversions') && response.request().method() === 'POST');
  await convert.click();
  const queued = await (await conversionResponse).json();
  await expect.poll(async () => {
    const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/conversions`)).json();
    const current = jobs.find(job => job.id === queued.id);
    if (current?.state === 'failed') throw new Error(`DVD conversion failed: ${current.error}`);
    return current?.state;
  }, { timeout: 45_000 }).toBe('needs_review');
  const { jobs: convertedJobs } = await (await page.request.get(`${baseURL}/api/v1/admin/conversions`)).json();
  const converted = convertedJobs.find(job => job.id === queued.id);
  expect(converted.report.input_sha256).toBe(originalHash);
  expect(converted.report.validation.output_decoded_frames).toBe(converted.report.validation.source_decoded_frames);
  expect(converted.movie_sha256).toMatch(/^[a-f0-9]{64}$/);
  expect(converted.progress.workflow).toBe('conversion');
  expect(converted.progress.stage).toBe(3);

  const convertedCard = page.locator(`[data-testid="conversion-job"][data-job-id="${queued.id}"]`);
  await expandImport(page, queued.id);
  await expect(convertedCard.getByRole('progressbar', { name: 'Validate output', exact: true })).toHaveAttribute('aria-valuenow', '100');
  await expect(convertedCard.getByRole('progressbar', { name: 'Register', exact: true })).toHaveAttribute('aria-valuenow', '0');
  await convertedCard.getByRole('textbox', { name: 'Movie title', exact: true }).fill('E2E Converted DVD');
  await convertedCard.getByRole('spinbutton', { name: 'Year (optional)', exact: true }).fill('2002');
  await convertedCard.getByRole('textbox', { name: 'Description', exact: true }).fill('Generated DVD fixture; not a NAS movie.');
  await convertedCard.getByRole('checkbox', { name: /I reviewed the validated conversion, warnings and metadata/ }).check();
  await convertedCard.getByRole('button', { name: 'Publish to local library', exact: true }).click();
  await expect.poll(async () => {
    const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/conversions`)).json();
    const current = jobs.find(job => job.id === queued.id);
    if (current?.state === 'failed') throw new Error(`DVD publication failed: ${current.error}`);
    return current?.state;
  }, { timeout: 30_000 }).toBe('published');
  await expandImport(page, queued.id, true);
  await expect(convertedCard.getByRole('progressbar', { name: 'Register', exact: true })).toHaveAttribute('aria-valuenow', '100');
  const library = await (await page.request.get(`${baseURL}/api/v1/movies`)).json();
  const published = library.movies.filter(movie => movie.title === 'E2E Converted DVD');
  expect(published).toHaveLength(1);
  expect(library.movies).toHaveLength(2);
  const range = await page.request.get(`${baseURL}/api/v1/movies/${published[0].id}/video`, { headers: { Range: 'bytes=0-31' } });
  expect(range.status()).toBe(206);
  expect((await range.body()).length).toBe(32);
  const browserMedia = page.waitForResponse(response =>
    response.url().endsWith(`/api/v1/movies/${published[0].id}/video`)
    && response.request().resourceType() === 'media');
  await convertedCard.getByRole('button', { name: 'Watch published movie', exact: true }).click();
  expect((await browserMedia).status()).toBe(206);
  const video = page.getByTestId('movie-player');
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.readyState)).toBeGreaterThanOrEqual(1);
  expect(await video.evaluate((element: HTMLVideoElement) => element.duration)).toBeGreaterThan(2);
  expect(await video.evaluate((element: HTMLVideoElement) => element.duration)).toBeLessThan(4);
  await video.evaluate(async (element: HTMLVideoElement) => { element.muted = true; await element.play(); });
  await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.currentTime)).toBeGreaterThan(0.25);

  expect(command('sha256sum', [dvdIncomingFile]).split(/\s+/, 1)[0]).toBe(originalHash);
  const retained = (await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json()).files.find(file => file.name === original.name);
  expect(retained.identity).toBe(original.identity);
  const media = join(artifactsRoot!, 'media');
  expect(readdirSync(media).some(packageName => existsSync(join(media, packageName, 'original-tracks.mkv')))).toBe(true);
});

async function compareInspectedProgram(page: Page, probeId: string) {
  const card = page.locator(`[data-testid="probe-job"][data-job-id="${probeId}"]`);
  const comparison = card.getByTestId('comparison-preview');
  await expect(comparison).toContainText('not lossless');
  await expect(comparison).toContainText('not motion detection');
  const mediaResponses: string[] = [];
  const listener = response => {
    if (response.url().includes('/admin/comparisons/') && response.request().resourceType() === 'media'
      && response.status() === 206) mediaResponses.push(response.url());
  };
  page.on('response', listener);
  try {
    const queued = page.waitForResponse(response => response.url().endsWith(`/admin/probes/${probeId}/comparison`)
      && response.request().method() === 'POST');
    await comparison.getByRole('button', { name: 'Generate Preserve vs Yadif preview', exact: true }).click();
    expect((await queued).status()).toBe(202);
    const session = await (await page.request.get(`${baseURL}/api/v1/auth/me`)).json();
    const path = `${baseURL}/api/v1/admin/probes/${probeId}/comparison`;
    const duplicate = await page.request.post(path, {
      headers: { Origin: baseURL, 'x-csrf-token': session.csrf_token },
    });
    expect(duplicate.status()).toBe(409);
    await expect.poll(async () => {
      const current = await (await page.request.get(path)).json();
      if (current?.state === 'failed') throw new Error(`Comparison failed: ${current.error}`);
      return current?.state;
    }, { timeout: 45_000 }).toBe('succeeded');
    const current = await (await page.request.get(path)).json();
    expect(current.progress.workflow).toBe('comparison');
    expect(current.progress.stage).toBe(7);
    expect(current.progress.total_units).toBeNull();
    expect(current.report.preserve.decoded_frames).toBe(current.report.yadif.decoded_frames);
    expect(current.report.yadif.filter).toBe('yadif=mode=send_frame:parity=auto:deint=all');
    expect(current.report.preserve.filter).toBeNull();
    await expect(comparison.locator('video')).toHaveCount(2);
    await expect(comparison.locator('.phase-progress')).toHaveText('Workflow complete.');
    await expect(comparison.getByRole('progressbar', { name: 'Finalize comparison', exact: true })).toHaveAttribute('aria-valuenow', '100');
    for (const treatment of ['preserve', 'yadif']) {
      const url = `${baseURL}/api/v1/admin/comparisons/${current.id}/${treatment}/video`;
      const range = await page.request.get(url, { headers: { Range: 'bytes=0-31' } });
      expect(range.status()).toBe(206);
      expect((await range.body()).length).toBe(32);
      const video = comparison.locator(`video[aria-label="${treatment === 'preserve' ? 'Preserve' : 'Yadif'} comparison"]`);
      await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.readyState)).toBeGreaterThanOrEqual(1);
      expect(await video.evaluate((element: HTMLVideoElement) => element.duration)).toBeGreaterThan(1);
      await video.evaluate(async (element: HTMLVideoElement) => { element.muted = true; await element.play(); });
      await expect.poll(() => video.evaluate((element: HTMLVideoElement) => element.currentTime)).toBeGreaterThan(0.25);
      await video.evaluate((element: HTMLVideoElement) => element.pause());
      await expect.poll(() => mediaResponses.some(url => url.includes(`/${current.id}/${treatment}/video`))).toBe(true);
    }
    return current;
  } finally {
    page.off('response', listener);
  }
}

test('administrator midpoint Preserve vs Yadif clips share a native-seeked window without publishing', async ({ page, browser }) => {
  test.setTimeout(90_000);
  await login(page, 'e2e-admin');
  const session = await (await page.request.get(`${baseURL}/api/v1/auth/me`)).json();
  const headers = { Origin: baseURL, 'x-csrf-token': session.csrf_token };
  const before = await (await page.request.get(`${baseURL}/api/v1/movies`)).json();
  const { jobs: scans } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
  // Reuse the verified fixture scan when running the suite; remain independently selectable.
  let scan = scans.find(job => job.name === 'browser-valid.iso' && job.state === 'needs_review');
  if (!scan) {
    const incoming = await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json();
    const file = incoming.files.find(file => file.name === 'browser-valid.iso');
    const queued = await page.request.post(`${baseURL}/api/v1/admin/scans`, {
      headers, data: { name: file.name, identity: file.identity, copy_complete: true },
    });
    expect(queued.status()).toBe(202);
    const pending = await queued.json();
    await expect.poll(async () => {
      const { jobs } = await (await page.request.get(`${baseURL}/api/v1/admin/scans`)).json();
      scan = jobs.find(job => job.id === pending.id);
      return scan?.state;
    }, { timeout: 30_000 }).toBe('needs_review');
  }
  const inspected = await page.request.post(`${baseURL}/api/v1/admin/scans/${scan.id}/inspect`, {
    headers, data: { title: 1, angle: 1 },
  });
  expect(inspected.status()).toBe(202);
  const probe = await inspected.json();
  expect(probe.attempt_id).toBe(scan.attempt_id);
  await expandImport(page, probe.id);
  const comparison = await compareInspectedProgram(page, probe.id);
  expect(comparison.report.requested_start_seconds).toBeGreaterThan(4);
  expect(comparison.report.requested_duration_seconds).toBe(10);
  expect(comparison.report.seek.native_dvd_seek).toContain('seeking: requested_nav_pts=');
  expect(comparison.report.full_disc_validated).toBe(false);
  expect(existsSync(join(artifactsRoot!, 'scratch', comparison.attempt_id, 'input.iso'))).toBe(false);
  const viewer = await browser.newContext();
  try {
    const viewerPage = await viewer.newPage();
    await login(viewerPage, 'e2e-viewer');
    for (const treatment of ['preserve', 'yadif']) {
      const response = await viewerPage.request.get(`${baseURL}/api/v1/admin/comparisons/${comparison.id}/${treatment}/video`);
      expect(response.status()).toBe(403);
    }
    await expect(viewerPage.getByTestId('comparison-preview')).toHaveCount(0);
  } finally {
    await viewer.close();
  }
  const { movies } = await (await page.request.get(`${baseURL}/api/v1/movies`)).json();
  expect(movies).toHaveLength(before.movies.length);
});
