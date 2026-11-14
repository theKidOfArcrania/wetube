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

test('failed resume save still allows returning to the library with a warning', async ({ page }) => {
  await login(page, 'e2e-viewer');
  await page.route('**/api/v1/movies/*/progress', async route => {
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
  await expect(page.getByTestId('latest-readiness').getByText('Required scan, inspection and conversion capabilities available. Optional OCR does not determine readiness.', { exact: true })).toBeVisible();
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

test('live media progress renders measurable, indeterminate, queued, review, failed and terminal stages honestly', async ({ page }) => {
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
    },
  };
  await page.route('**/api/v1/admin/imports?*', route => {
    const history = new URL(route.request().url()).searchParams.get('history') === 'true';
    const entries = [
      { item: { kind: 'scan', job: scan }, can_archive: scan.state === 'failed', history: false },
      { item: { kind: 'conversion', job: conversion }, can_archive: ['failed', 'published'].includes(conversion.state),
        history: conversion.state === 'published' },
    ];
    return route.fulfill({ json: { entries: entries.filter(entry => entry.history === history),
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
  await expect(convertedCard.getByText('Total workflow — stage-based, not elapsed-time weighted', { exact: true })).toBeVisible();

  conversion.progress.completed_units = 80;
  await expect(encode).toHaveAttribute('aria-valuenow', '99');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('remaining');
  conversion.progress.total_units = null;
  await expect(encode).not.toHaveAttribute('aria-valuenow');
  await expect(convertedCard.locator('.phase-progress')).toContainText('indeterminate');

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
  conversion.progress.updated_at = now() - 60;
  await expect(convertedCard.locator('.phase-progress')).toContainText('last recorded; not live');
  await expect(convertedCard.locator('.phase-progress')).not.toContainText('remaining');
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
    expect(current.report.preserve.decoded_frames).toBe(current.report.yadif.decoded_frames);
    expect(current.report.yadif.filter).toBe('yadif=mode=send_frame:parity=auto:deint=all');
    expect(current.report.preserve.filter).toBeNull();
    await expect(comparison.locator('video')).toHaveCount(2);
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
