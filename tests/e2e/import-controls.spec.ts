import { expect, test } from '@playwright/test';
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { lstatSync, mkdtempSync, readFileSync, realpathSync, rmSync } from 'node:fs';
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
let frontend: string;

test.beforeAll(() => {
  test.setTimeout(180_000);
  const requested = process.env.WETUBE_E2E_ARTIFACTS_DIR;
  if (!requested || lstatSync(requested).isSymbolicLink()) {
    throw new Error('Set WETUBE_E2E_ARTIFACTS_DIR to an existing non-symlink directory under /mnt/c/TempPrivate.');
  }
  const base = realpathSync(requested);
  const location = relative(realpathSync('/mnt/c/TempPrivate'), base);
  if (location === '..' || location.startsWith(`..${sep}`) || isAbsolute(location)) {
    throw new Error('Browser fixtures must remain under /mnt/c/TempPrivate.');
  }
  frontend = mkdtempSync(join(base, 'wetube-import-controls-'));
  const result = spawnSync('rustup', [
    'run', 'stable', '/home/henry/.cargo/bin/trunk', 'build', '--release', '--locked', '--dist', frontend,
  ], {
    cwd: join(repo, 'crates/web'), env: process.env, encoding: 'utf8', timeout: 180_000,
    maxBuffer: 4 * 1024 * 1024,
  });
  if (result.error || result.status !== 0) {
    throw new Error(`Frontend build failed: ${result.error ?? result.stderr}`);
  }
});

test.afterAll(() => {
  if (frontend) rmSync(frontend, { recursive: true, force: true });
});

test('Simple inspection and conversion controls offer only available actions', async ({ page }) => {
  const origin = 'http://wetube.test';
  const scanId = randomUUID();
  const probeId = randomUUID();
  const siblingId = randomUUID();
  const conversionId = randomUUID();
  const attempt = randomUUID();
  const csrf = 'isolated-browser-fixture';
  const now = Math.floor(Date.now() / 1000);
  const scan = {
    id: scanId, name: 'fixture.iso', state: 'needs_review', simple_state: 'inspection',
    simple_message: null, created_at: now, attempt_id: attempt, error: null,
    report: { titles: [{
      title: 1, duration_seconds: 60, chapter_count: 1, audio_count: 1, subtitle_count: 0,
      audio_languages: ['en'], subtitle_languages: [], angles: [1], main_feature_candidate: true,
    }, {
      title: 2, duration_seconds: 10, chapter_count: 1, audio_count: 1, subtitle_count: 0,
      audio_languages: ['en'], subtitle_languages: [], angles: [1], main_feature_candidate: false,
    }] },
  };
  const probe = {
    id: probeId, name: scan.name, title: 1, angle: 1, state: 'needs_review',
    created_at: now, attempt_id: attempt, error: null,
    report: { sha256: 'a'.repeat(64), inspection: { warnings: [], metadata: {
      format: { duration: '60' }, streams: [{ codec_type: 'video', codec_name: 'mpeg2video' }],
    } } },
  };
  let conversionState: string | undefined;
  const sibling = { ...probe, id: siblingId, title: 2 };
  let advancedRequests = 0;
  let conversionRequests = 0;
  let publicationRequests = 0;
  const conversion = () => ({
    id: conversionId, probe_job_id: probeId, state: conversionState, created_at: now,
    attempt_id: randomUUID(), video_treatment: 'preserve', phase: conversionState,
    report: conversionState === 'needs_review' ? { status: 'verified_experimental_output', warnings: [] } : null,
    error: conversionState === 'failed' ? 'Fixture conversion failed; input retained.' : null,
    movie_id: null, movie_sha256: conversionState === 'needs_review' ? 'b'.repeat(64) : null,
  });
  const pageErrors: string[] = [];
  page.on('pageerror', error => pageErrors.push(error.message));

  // Serve the real compiled frontend through browser routes: no database, media, server or worker.
  await page.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin !== origin) return route.abort();
    if (!url.pathname.startsWith('/api/')) {
      const name = url.pathname === '/' ? 'index.html' : url.pathname.slice(1);
      if (name.includes('/') || name.includes('..')) return route.abort();
      const contentType = name.endsWith('.wasm') ? 'application/wasm'
        : name.endsWith('.js') ? 'text/javascript'
          : name.endsWith('.css') ? 'text/css'
            : name.endsWith('.svg') ? 'image/svg+xml' : 'text/html';
      return route.fulfill({ contentType, body: readFileSync(join(frontend, name)) });
    }
    const path = url.pathname;
    if (path === '/api/v1/auth/me') {
      return route.fulfill({ json: { user: { id: 'admin', username: 'fixture-admin', role: 'admin' }, csrf_token: csrf } });
    }
    if (path === '/api/v1/movies') return route.fulfill({ json: { movies: [] } });
    if (path === '/api/v1/status') return route.fulfill({ json: {
      version: '0.1.0', worker_last_seen: now, conversion_enabled: true, iso_deletion_enabled: false,
    } });
    if (path === '/api/v1/jobs' || path === '/api/v1/admin/trash') return route.fulfill({ json: [] });
    if (path === '/api/v1/admin/queue') return route.fulfill({ json: { revision: 0, paused: false, maximum: 1, jobs: [] } });
    if (path === '/api/v1/admin/incoming') return route.fulfill({ json: { configured: true, files: [] } });
    if (path === '/api/v1/admin/metadata') return route.fulfill({ json: { configured: false } });
    if (path === '/api/v1/admin/imports') {
      const entries = [
        { item: { kind: 'scan', job: scan }, can_archive: false },
        { item: { kind: 'probe', job: probe }, can_archive: false },
        { item: { kind: 'probe', job: sibling }, can_archive: false },
        ...(conversionState ? [{ item: { kind: 'conversion', job: conversion() }, can_archive: false }] : []),
      ];
      return route.fulfill({ json: { entries, total: entries.length, history_count: 0, next: null } });
    }
    if (path === `/api/v1/admin/scans/${scanId}/advanced`) {
      expect(route.request().method()).toBe('POST');
      expect(route.request().headers()['x-csrf-token']).toBe(csrf);
      advancedRequests++;
      scan.simple_state = 'advanced';
      return route.fulfill({ json: scan });
    }
    if (path === '/api/v1/admin/conversions') {
      conversionRequests++;
      return route.fulfill({ status: 409, json: { error: 'Unexpected conversion request' } });
    }
    if (path === `/api/v1/admin/conversions/${conversionId}/publish`) {
      publicationRequests++;
      return route.fulfill({ status: 409, json: { error: 'Unexpected publication request' } });
    }
    return route.fulfill({ status: 404, json: { error: 'No comparison available' } });
  });
  await page.goto(origin);
  const probeCard = page.locator(`[data-testid="probe-job"][data-job-id="${probeId}"]`);
  await page.locator(`[data-testid="attention-row"][data-job-id="${probeId}"] > details.import-details > summary`).click();
  const advanced = probeCard.getByRole('button', { name: 'Use Advanced for this import', exact: true });
  const siblingCard = page.locator(`[data-testid="probe-job"][data-job-id="${siblingId}"]`);
  await page.locator(`[data-testid="attention-row"][data-job-id="${siblingId}"] > details.import-details > summary`).click();
  await expect(advanced).toBeVisible();
  await expect(probeCard.getByRole('button', { name: 'Convert reviewed title', exact: true })).toHaveCount(0);
  await advanced.click();
  await expect(probeCard.getByRole('combobox', { name: 'Video treatment', exact: true })).toBeVisible();
  await probeCard.getByRole('combobox', { name: 'Video treatment', exact: true }).selectOption('yadif');
  await probeCard.getByRole('checkbox', { name: /I reviewed this inspection and warnings/ }).check();
  await expect(probeCard.getByRole('button', { name: 'Convert reviewed title', exact: true })).toBeEnabled();
  expect(advancedRequests).toBe(1);

  scan.simple_state = 'conversion';
  conversionState = 'queued';
  await expect(probeCard.getByText('Conversion queued — waiting for worker.', { exact: true })).toBeVisible();
  await expect(siblingCard.getByText('Conversion queued — waiting for worker.', { exact: true })).toBeVisible();
  await expect(siblingCard.getByRole('button', { name: 'Use Advanced for this import', exact: true })).toHaveCount(0);
  await expect(siblingCard.getByRole('button', { name: 'Convert reviewed title', exact: true })).toHaveCount(0);
  await expect(advanced).toHaveCount(0);
  await expect(probeCard.getByRole('button', { name: 'Convert reviewed title', exact: true })).toHaveCount(0);
  conversionState = 'converting';
  await expect(probeCard.getByText('Conversion running — not published.', { exact: true })).toBeVisible();
  conversionState = 'needs_review';
  await expect(probeCard.getByText(/Conversion complete — review validation and metadata/)).toBeVisible();
  await expect(siblingCard.getByText(/Conversion complete — review validation and metadata/)).toBeVisible();
  await expect(advanced).toHaveCount(0);
  const conversionCard = page.locator(`[data-testid="conversion-job"][data-job-id="${conversionId}"]`);
  await page.locator(`[data-testid="attention-row"][data-job-id="${conversionId}"] > details.import-details > summary`).click();
  const publish = conversionCard.getByRole('button', { name: 'Publish to local library', exact: true });
  await expect(publish).toBeDisabled();
  await conversionCard.getByRole('textbox', { name: 'Movie title', exact: true }).fill('Reviewed fixture');
  await expect(publish).toBeDisabled();
  await conversionCard.getByRole('checkbox', { name: /I reviewed the validated conversion, warnings and metadata/ }).check();
  await expect(publish).toBeEnabled();

  conversionState = 'failed';
  await expect(probeCard.getByText(/Conversion failed — inputs and reports retained/)).toBeVisible();
  await expect(siblingCard.getByText(/Conversion failed — inputs and reports retained/)).toBeVisible();
  await expect(conversionCard.getByText('Fixture conversion failed; input retained.', { exact: true })).toBeVisible();
  await expect(publish).toHaveCount(0);
  await expect(advanced).toHaveCount(0);
  expect(conversionRequests).toBe(0);
  expect(publicationRequests).toBe(0);
  expect(pageErrors).toEqual([]);
});

test('Processing queue controls reconcile conflicts and require cancellation approval', async ({ page }) => {
  const origin = 'http://wetube.test';
  const csrf = 'isolated-queue-browser';
  const queue = {
    revision: 1, paused: false, maximum: 2,
    jobs: [
      { key: 1, kind: 'diagnostic', id: 'active', name: 'Tool diagnostics', state: 'running', reason: 'Processing' },
      { key: 2, kind: 'scan', id: 'scan', name: 'Waiting scan.iso', state: 'waiting', reason: 'Waiting for worker' },
      { key: 3, kind: 'publication', id: 'publication', name: 'Waiting publication.iso', state: 'waiting', reason: 'Waiting for worker' },
    ],
  };
  let conflict = true;
  let viewer = false;
  let cancellations = 0;
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin !== origin) return route.abort();
    if (!url.pathname.startsWith('/api/')) {
      const name = url.pathname === '/' ? 'index.html' : url.pathname.slice(1);
      if (name.includes('/') || name.includes('..')) return route.abort();
      return route.fulfill({ body: readFileSync(join(frontend, name)), contentType:
        name.endsWith('.wasm') ? 'application/wasm' : name.endsWith('.js') ? 'text/javascript'
          : name.endsWith('.css') ? 'text/css' : 'text/html' });
    }
    const path = url.pathname;
    if (path === '/api/v1/auth/me') return route.fulfill({ json: { user: { id: viewer ? 'viewer' : 'admin', username: viewer ? 'queue-viewer' : 'queue-admin', role: viewer ? 'viewer' : 'admin' }, csrf_token: csrf } });
    if (path === '/api/v1/movies') return route.fulfill({ json: { movies: [] } });
    if (path === '/api/v1/status') return route.fulfill({ json: { version: '0.1.0', worker_last_seen: null, conversion_enabled: false, iso_deletion_enabled: false } });
    if (path === '/api/v1/jobs' || path === '/api/v1/admin/trash') return route.fulfill({ json: [] });
    if (path === '/api/v1/admin/incoming') return route.fulfill({ json: { configured: false, files: [] } });
    if (path === '/api/v1/admin/metadata') return route.fulfill({ json: { configured: false } });
    if (path === '/api/v1/admin/imports') return route.fulfill({ json: { entries: [], total: 0, history_count: 0, next: null } });
    if (path === '/api/v1/admin/queue') return route.fulfill({ json: queue });
    expect(route.request().method()).toBe('POST');
    expect(route.request().headers()['x-csrf-token']).toBe(csrf);
    if (path === '/api/v1/admin/queue/settings') {
      const body = route.request().postDataJSON();
      expect(body.revision).toBe(queue.revision);
      expect(body).not.toHaveProperty('threads');
      queue.paused = body.paused;
      queue.maximum = body.maximum;
    } else if (path === '/api/v1/admin/queue/order') {
      const body = route.request().postDataJSON();
      expect(body.waiting).not.toContain(1);
      if (conflict) {
        conflict = false;
        queue.revision++;
        queue.jobs.push({ key: 4, kind: 'probe', id: 'new', name: 'New inspection.iso', state: 'waiting', reason: 'Waiting for worker' });
        return route.fulfill({ status: 409, json: { error: 'Queue changed. Refresh and try again.' } });
      }
      expect(body.waiting).toHaveLength(3);
      expect(body.revision).toBe(queue.revision);
      queue.jobs.sort((a, b) => a.state === 'running' ? -1 : b.state === 'running' ? 1 : body.waiting.indexOf(a.key) - body.waiting.indexOf(b.key));
    } else if (path.endsWith('/cancel')) {
      cancellations++;
      const key = Number(path.split('/').at(-2));
      const job = queue.jobs.find(job => job.key === key)!;
      job.state = job.state === 'running' ? 'cancelling' : 'cancelled';
      job.reason = job.state === 'cancelling' ? 'Cancelling; waiting for owned work to stop' : 'Cancelled; retained outputs unchanged';
    } else return route.fulfill({ status: 404, json: { error: 'Unexpected request' } });
    queue.revision++;
    return route.fulfill({ json: queue });
  });
  await page.goto(origin);
  const panel = page.getByRole('region', { name: 'Processing queue', exact: true });
  await expect(panel).toBeVisible();
  await panel.getByRole('button', { name: 'Pause new starts', exact: true }).click();
  await expect(panel.getByRole('button', { name: 'Resume new starts', exact: true })).toBeVisible();
  await panel.getByRole('spinbutton', { name: 'Simultaneous jobs', exact: true }).fill('1');
  await panel.getByRole('button', { name: 'Apply job limit', exact: true }).click();
  await expect(panel.getByRole('status')).toContainText('limit 1 · 1 active');
  const publication = panel.locator('li').filter({ hasText: 'Waiting publication.iso' });
  await publication.getByRole('button', { name: 'Move waiting job earlier', exact: true }).click();
  await expect(panel.getByRole('alert')).toHaveText('Queue changed. Refresh and try again.');
  await expect(panel.getByText('New inspection.iso', { exact: true })).toBeVisible();
  await publication.getByRole('button', { name: 'Move waiting job earlier', exact: true }).click();
  await expect(panel.locator('li').nth(1)).toContainText('Waiting publication.iso');
  await panel.getByRole('button', { name: 'Resume new starts', exact: true }).click();
  await expect(panel.getByRole('button', { name: 'Pause new starts', exact: true })).toBeVisible();
  await panel.getByRole('button', { name: 'Cancel active job', exact: true }).click();
  expect(cancellations).toBe(0);
  await panel.getByRole('button', { name: 'Keep job', exact: true }).click();
  await panel.getByRole('button', { name: 'Cancel active job', exact: true }).click();
  await panel.getByRole('button', { name: 'Confirm cancellation', exact: true }).click();
  await expect(panel.getByText('Cancelling; waiting for owned work to stop', { exact: true })).toBeVisible();
  expect(cancellations).toBe(1);
  await publication.getByRole('button', { name: 'Cancel waiting job', exact: true }).click();
  await publication.getByRole('button', { name: 'Confirm cancellation', exact: true }).click();
  await expect(panel.getByText('Cancelled; retained outputs unchanged', { exact: true })).toBeVisible();
  expect(cancellations).toBe(2);
  viewer = true;
  await page.reload();
  await expect(page.getByText('Signed in as queue-viewer', { exact: true })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Processing queue', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Pause new starts', exact: true })).toHaveCount(0);
  expect(errors).toEqual([]);
});
