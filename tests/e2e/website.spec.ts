import { expect, test, type Page } from '@playwright/test';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { randomBytes, randomUUID } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const title = 'E2E Test Movie';
const password = randomBytes(24).toString('base64url');
let root: string | undefined;
let server: ChildProcess | undefined;
let worker: ChildProcess | undefined;
let baseURL: string;
let movieId: string;
let serverLog = '';
let retainState = false;
let incomingFile: string;

function command(program: string, args: string[], input?: string, cwd = repo): string {
  const result = spawnSync(program, args, {
    cwd,
    input,
    encoding: 'utf8',
    timeout: 180_000,
    maxBuffer: 4 * 1024 * 1024,
  });
  if (result.error || result.status !== 0) {
    throw new Error(`${program} failed: ${result.error ?? result.stderr}`);
  }
  return result.stdout.trim();
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
  retainState = testInfo.config.metadata.retainState === true;
  command('cargo', ['build', '--locked', '--package', 'wetube-server', '--package', 'wetube-worker']);
  command('rustup', ['run', 'stable', '/home/henry/.cargo/bin/trunk', 'build', '--release', '--locked'], undefined, join(repo, 'crates/web'));
  root = mkdtempSync(join(tmpdir(), 'wetube-browser-e2e-'));
  if (retainState) {
    const credentials = join(root, 'login.txt');
    writeFileSync(credentials, `Username: e2e-admin\nPassword: ${password}\nViewer: e2e-viewer (same test password)\n`, {
      flag: 'wx', mode: 0o600,
    });
    console.log(`Retained preview directory: ${root}`);
    console.log(`Local test credentials: ${credentials}`);
  }
  const state = join(root, 'state');
  const media = join(root, 'media');
  const incoming = join(root, 'incoming');
  const scratch = join(root, 'scratch');
  mkdirSync(incoming, { mode: 0o700 });
  incomingFile = join(incoming, 'browser-invalid.iso');
  const attempt = randomUUID();
  const attemptDir = join(root, attempt);
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
  command('safe-exec', [
    join(repo, 'tests/e2e/fixture-report.rhai'), report, incomingFile, '--arg', `attempt=${attempt}`,
  ]);
  const binary = join(repo, 'target/debug/wetube-server');
  command(binary, ['--state-dir', state, 'init']);
  command(binary, ['--state-dir', state, 'add-user', '--username', 'e2e-admin', '--admin', '--password-stdin'], `${password}\n`);
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
    '--static-dir', join(repo, 'dist'), '--media-dir', media, '--incoming-dir', incoming,
  ], { cwd: repo, stdio: ['ignore', 'pipe', 'pipe'] });
  server.stdout?.on('data', chunk => { serverLog = (serverLog + chunk).slice(-16_384); });
  server.stderr?.on('data', chunk => { serverLog = (serverLog + chunk).slice(-16_384); });
  worker = spawn(join(repo, 'target/debug/wetube-worker'), [
    '--state-dir', state, 'run', '--incoming-dir', incoming, '--scratch-dir', scratch,
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
  if (root && !retainState) rmSync(root, { recursive: true, force: true });
});

async function login(page: Page, username: string) {
  await page.goto(baseURL);
  await page.getByLabel('Username', { exact: true }).fill(username);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Sign out', exact: true })).toBeVisible();
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

test('admin diagnostics and ISO selection reach persisted worker results without publication', async ({ page }) => {
  await login(page, 'e2e-admin');
  await expect(page.getByText('Worker connected', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Run read-only diagnostics', exact: true }).click();
  await expect.poll(async () => {
    const jobs = await (await page.request.get(`${baseURL}/api/v1/jobs`)).json();
    return jobs.some(job => job.state === 'succeeded' || job.state === 'failed');
  }).toBe(true);
  await expect(page.getByText(/Media prerequisites (available|need attention)/)).toBeVisible();
  const original = (await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json()).files[0];
  const originalHash = command('sha256sum', [incomingFile]).split(/\s+/, 1)[0];
  await page.getByRole('button', { name: 'Refresh incoming files', exact: true }).click();
  await page.getByLabel('Completed ISO copy', { exact: true }).selectOption({ index: 1 });
  await page.getByLabel('DVD title (1-99)', { exact: true }).fill('1');
  await page.getByLabel('Angle (1-9)', { exact: true }).fill('1');
  await page.getByRole('checkbox', { name: /I confirm this copy is finished/ }).check();
  await page.getByRole('button', { name: 'Stage and inspect selected title', exact: true }).click();
  await expect.poll(async () => {
    const jobs = await (await page.request.get(`${baseURL}/api/v1/admin/probes`)).json();
    return jobs.some(job => job.name === 'browser-invalid.iso' && job.state === 'failed');
  }).toBe(true);
  await expect(page.getByText('Failed - inputs retained', { exact: true })).toBeVisible();
  const jobs = await (await page.request.get(`${baseURL}/api/v1/admin/probes`)).json();
  expect(jobs.find(job => job.name === 'browser-invalid.iso').report.sha256).toBe(originalHash);
  expect(command('sha256sum', [incomingFile]).split(/\s+/, 1)[0]).toBe(originalHash);
  const retained = (await (await page.request.get(`${baseURL}/api/v1/admin/incoming`)).json()).files[0];
  expect(retained.identity).toBe(original.identity);
  expect((await (await page.request.get(`${baseURL}/api/v1/movies`)).json()).movies).toHaveLength(1);
});
