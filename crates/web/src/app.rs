use gloo_net::http::{Request, Response};
use leptos::{prelude::*, task::spawn_local};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wetube_core::{
    ApiError, ArchiveImport, ComparisonJob, ConversionJob, ConversionRequest, ConversionState,
    DiagnosticJob, DiagnosticReport, ImportEntry, ImportJob, ImportPage, IncomingFiles,
    InspectRequest, JobState, Library, LoginRequest, MediaProgress, MediaProgressState,
    MediaWorkflow, MetadataMatch, MetadataResults, MetadataSearch, MetadataStatus, Movie,
    MovieDetails, PlaybackProgress, ProbeJob, ProbeState, PublishRequest, Role, ScanJob,
    ScanRequest, ScanState, Session, SystemStatus, TmdbSource, UpdatePlaybackProgress,
    VideoTreatment,
};

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    if let Some(date) = response.headers().get("date") {
        let server_seconds = js_sys::Date::parse(&date) / 1000.0;
        if server_seconds.is_finite() {
            SERVER_CLOCK.with(|clock| clock.set(Some((server_seconds, browser_clock()))));
        }
    }
    if !response.ok() {
        return Err(response
            .json::<ApiError>()
            .await
            .map(|error| error.error)
            .unwrap_or_else(|_| format!("Request failed ({})", response.status())));
    }
    response.json().await.map_err(|error| error.to_string())
}

thread_local! {
    static SERVER_CLOCK: Cell<Option<(f64, f64)>> = const { Cell::new(None) };
    // Polling replaces job cards; retain the receipt time for an unchanged observation.
    static PROGRESS_RECEIPTS: RefCell<HashMap<String, ProgressReceipt>> = RefCell::new(HashMap::new());
}

struct ProgressReceipt {
    progress: MediaProgress,
    received: f64,
    heartbeat_received: Option<f64>,
}

fn heartbeat_receipt(updated_at: i64, received: f64, heartbeat_advanced: bool) -> Option<f64> {
    // Server time avoids browser clock skew. Without it, require an observed heartbeat
    // before treating the first snapshot as live.
    SERVER_CLOCK
        .with(|clock| {
            clock.get().map(|(server_seconds, observed)| {
                let server_now = server_seconds + (received - observed).max(0.0);
                received - (server_now - updated_at as f64)
            })
        })
        .or_else(|| heartbeat_advanced.then_some(received))
}

#[component]
fn MediaJobProgress(
    observation_key: String,
    workflow: MediaWorkflow,
    progress: Option<MediaProgress>,
    state: MediaProgressState,
) -> impl IntoView {
    let workflow = progress.as_ref().map_or(workflow, |p| p.workflow);
    let fills = workflow.stage_fills(progress.as_ref(), state);
    let received = browser_clock();
    let (received, heartbeat_received) = PROGRESS_RECEIPTS.with(|receipts| {
        let mut receipts = receipts.borrow_mut();
        if receipts.len() > 256 {
            receipts.clear();
        }
        progress.as_ref().map_or((received, None), |p| {
            let entry = receipts
                .entry(observation_key)
                .or_insert_with(|| ProgressReceipt {
                    progress: p.clone(),
                    received,
                    heartbeat_received: heartbeat_receipt(p.updated_at, received, false),
                });
            if entry.progress.updated_at != p.updated_at {
                entry.heartbeat_received = heartbeat_receipt(
                    p.updated_at,
                    received,
                    p.updated_at > entry.progress.updated_at,
                );
            }
            if entry.progress != *p {
                entry.progress = p.clone();
                entry.received = received;
            }
            (entry.received, entry.heartbeat_received)
        })
    });
    let now = RwSignal::new(browser_clock());
    if state == MediaProgressState::Active {
        let timer = gloo_timers::callback::Interval::new(1000, move || {
            let _ = now.try_set(browser_clock());
        });
        let timer = StoredValue::new_local(Some(timer));
        on_cleanup(move || {
            let _ = timer.try_update_value(|timer| timer.take());
        });
    }
    let message = move || {
        let active = state == MediaProgressState::Active
            && heartbeat_received
                .is_some_and(|heartbeat| (0.0..=10.0).contains(&(now.get() - heartbeat)));
        match state {
            MediaProgressState::Queued => "Queued — waiting for worker.".to_owned(),
            MediaProgressState::QueuedPublication => {
                "Publication queued — no publication ETA yet.".to_owned()
            }
            MediaProgressState::Review => {
                "Awaiting administrator review — not published.".to_owned()
            }
            MediaProgressState::Complete => "Workflow complete.".to_owned(),
            _ => progress.as_ref().map_or_else(
                || "Progress unavailable.".to_owned(),
                |p| {
                    let percentage = p
                        .fraction()
                        .map(|n| format!("{:.0}%", (n * 100.0).floor()))
                        .unwrap_or_else(|| "indeterminate".into());
                    let mut text = format!(
                        "{} · {} · elapsed {:.0}s",
                        p.phase, percentage, p.elapsed_seconds
                    );
                    if active {
                        let mut observed = p.clone();
                        observed.elapsed_seconds += (now.get() - received).max(0.0);
                        // Old records have no measurement anchor; do not recalculate their rate
                        // from the browser timer.
                        observed.measured_elapsed_seconds =
                            p.measured_elapsed_seconds.or(Some(p.elapsed_seconds));
                        if let Some((rate, eta)) = observed.rate_and_eta() {
                            if eta > 0.0 {
                                text.push_str(&format!(
                                    " · {:.1} {}/s · estimated {:.0}s remaining in this phase",
                                    rate,
                                    p.unit,
                                    eta.ceil()
                                ));
                            } else {
                                text.push_str(" · waiting for updated estimate");
                            }
                        }
                    } else {
                        text.push_str(" · last recorded; not live");
                    }
                    text
                },
            ),
        }
    };
    view! {
        <section class="media-progress" data-testid="media-progress">
            <p class="subtle">"Progress by stage, not time remaining"</p>
            <div class="stage-segments" role="group" aria-label="Total workflow stages">
                {workflow.stages().iter().zip(fills).map(|(label, fill)| {
                    let indeterminate = fill.is_none();
                    let width = format!("width: {}%", fill.unwrap_or(0.0) * 100.0);
                    let label = *label;
                    view! {
                        <div class="stage-segment">
                            <span class="stage-label">{label}</span>
                            <div class="stage-track" class:indeterminate=indeterminate
                                role="progressbar" aria-label=label aria-valuemin="0" aria-valuemax="100"
                                aria-valuenow=fill.map(|n| format!("{:.0}", n * 100.0))
                                aria-valuetext=if indeterminate { "Indeterminate" } else { "Stage progress" }>
                                <span class="stage-fill" style=width></span>
                            </div>
                        </div>
                    }
                }).collect_view()}
            </div>
            <p class="phase-progress" role="status">{message}</p>
        </section>
    }
}

async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let response = Request::get(path)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode(response).await
}

#[derive(Default)]
struct AdminRequests {
    closed: bool,
    next_id: usize,
    controllers: HashMap<usize, web_sys::AbortController>,
}

type AdminScope = StoredValue<Rc<RefCell<AdminRequests>>, LocalStorage>;

fn admin_scope() -> AdminScope {
    let scope = StoredValue::new_local(Rc::new(RefCell::new(AdminRequests::default())));
    on_cleanup(move || {
        let requests = scope.get_value();
        let mut requests = requests.borrow_mut();
        requests.closed = true;
        for controller in requests.controllers.values() {
            controller.abort();
        }
        requests.controllers.clear();
    });
    scope
}

async fn admin_request<T: DeserializeOwned>(
    scope: AdminScope,
    build: impl FnOnce(&web_sys::AbortSignal) -> Result<Request, String>,
) -> Result<T, String> {
    let requests = scope
        .try_get_value()
        .ok_or_else(|| "This page is no longer active.".to_owned())?;
    let controller = web_sys::AbortController::new()
        .map_err(|_| "This browser cannot initialize cancellable requests.".to_owned())?;
    let id = {
        let mut requests = requests.borrow_mut();
        if requests.closed {
            return Err("This page is no longer active.".into());
        }
        let id = requests.next_id;
        requests.next_id += 1;
        requests.controllers.insert(id, controller.clone());
        id
    };
    let deadline_controller = controller.clone();
    let deadline = gloo_timers::callback::Timeout::new(20_000, move || {
        deadline_controller.abort();
    });
    let result = async {
        let response = build(&controller.signal())?.send().await.map_err(|_| {
            "Request interrupted or timed out. Refresh job status before retrying.".to_owned()
        })?;
        decode(response).await
    }
    .await;
    drop(deadline);
    requests.borrow_mut().controllers.remove(&id);
    result
}

async fn admin_get<T: DeserializeOwned>(scope: AdminScope, path: &str) -> Result<T, String> {
    admin_request(scope, |signal| {
        Request::get(path)
            .abort_signal(Some(signal))
            .build()
            .map_err(|error| error.to_string())
    })
    .await
}

async fn admin_post<T: DeserializeOwned>(
    scope: AdminScope,
    path: &str,
    csrf: &str,
    body: &impl serde::Serialize,
) -> Result<T, String> {
    admin_request(scope, |signal| {
        Request::post(path)
            .header("x-csrf-token", csrf)
            .abort_signal(Some(signal))
            .json(body)
            .map_err(|error| error.to_string())
    })
    .await
}

#[component]
pub fn App() -> impl IntoView {
    let session = RwSignal::new(None::<Session>);
    let checking = RwSignal::new(true);
    spawn_local(async move {
        if let Ok(current) = get::<Session>("/api/v1/auth/me").await {
            session.set(Some(current));
        }
        checking.set(false);
    });
    view! {
        <header>
            <div class="brand">"WeTube"</div>
            <span class="subtle">"Your movies. Your NAS."</span>
        </header>
        <main>
            {move || {
                if checking.get() {
                    view! { <p class="subtle">"Connecting to your library..."</p> }.into_any()
                } else if let Some(current) = session.get() {
                    view! { <LibraryPage current=current session=session/> }.into_any()
                } else {
                    view! { <LoginPage session=session/> }.into_any()
                }
            }}
        </main>
        <footer>
                <h2>"About / Credits"</h2>
                <img src="/tmdb.svg" width="90" alt="TMDB"/>
                <p>"This application uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB."</p>
        </footer>
    }
}

#[component]
fn LoginPage(session: RwSignal<Option<Session>>) -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let submit = move |event: web_sys::SubmitEvent| {
        event.prevent_default();
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let credentials = LoginRequest {
            username: username.get_untracked(),
            password: password.get_untracked(),
        };
        spawn_local(async move {
            let result = async {
                let response = Request::post("/api/v1/auth/login")
                    .json(&credentials)
                    .map_err(|error| error.to_string())?
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                decode::<Session>(response).await
            }
            .await;
            busy.set(false);
            match result {
                Ok(current) => {
                    password.set(String::new());
                    session.set(Some(current));
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    view! {
        <section class="panel login">
            <span class="eyebrow">"Private household library"</span>
            <h1>"Welcome home."</h1>
            <p class="subtle">"Sign in with an account created by your administrator."</p>
            <form on:submit=submit>
                <label for="username">"Username"</label>
                <input id="username" name="username" autocomplete="username"
                    maxlength="64" required bind:value=username/>
                <label for="password">"Password"</label>
                <input id="password" name="password" type="password"
                    autocomplete="current-password" maxlength="1024" required bind:value=password/>
                <button type="submit" disabled=move || busy.get()>
                    {move || if busy.get() { "Signing in..." } else { "Sign in" }}
                </button>
            </form>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </section>
    }
}

#[derive(Clone, Copy)]
struct LibraryRevision(RwSignal<u64>);

#[component]
fn LibraryPage(current: Session, session: RwSignal<Option<Session>>) -> impl IntoView {
    let is_admin = current.user.role == Role::Admin;
    let username = current.user.username.clone();
    let csrf = StoredValue::new(current.csrf_token);
    let movies = RwSignal::new(Vec::<Movie>::new());
    let library_revision = RwSignal::new(0_u64);
    provide_context(LibraryRevision(library_revision));
    let selected = RwSignal::new(None::<Movie>);
    let editing_labels = RwSignal::new(None::<Movie>);
    let category = RwSignal::new(String::new());
    let tag = RwSignal::new(String::new());
    let loading = RwSignal::new(true);
    let signing_out = RwSignal::new(false);
    let playback_notice = RwSignal::new(None::<String>);
    let playback_owner = Owner::current().expect("LibraryPage must have a reactive owner");
    let watch_movie = Callback::new(move |movie: Movie| {
        playback_owner.with(|| {
            playback_notice.set(None);
            selected.set(Some(movie));
        });
    });
    let status = RwSignal::new(None::<SystemStatus>);
    let jobs = RwSignal::new(Vec::<DiagnosticJob>::new());
    let error = RwSignal::new(None::<String>);
    let query = RwSignal::new(String::new());
    let requesting = RwSignal::new(false);
    let filtered = Signal::derive(move || {
        let filter = wetube_core::LibraryFilter {
            q: query.get(),
            category: category.get(),
            tag: tag.get(),
        };
        movies
            .get()
            .into_iter()
            .filter(|movie| movie.matches_library(&filter))
            .collect::<Vec<_>>()
    });
    let mounted = Arc::new(AtomicBool::new(true));
    let cleanup = mounted.clone();
    on_cleanup(move || cleanup.store(false, Ordering::Relaxed));

    spawn_local(async move {
        while mounted.load(Ordering::Relaxed) {
            let revision = library_revision.get_untracked();
            let library = get::<Library>("/api/v1/movies").await;
            let system = get::<SystemStatus>("/api/v1/status").await;
            let history = if is_admin {
                Some(get::<Vec<DiagnosticJob>>("/api/v1/jobs").await)
            } else {
                None
            };
            if !mounted.load(Ordering::Relaxed) {
                break;
            }
            match library {
                Ok(library) if revision == library_revision.get_untracked() => {
                    movies.set(library.movies);
                    error.set(None);
                }
                Ok(_) => {}
                Err(message) => error.set(Some(message)),
            }
            loading.set(false);
            if let Ok(system) = system {
                status.set(Some(system));
            }
            if let Some(Ok(history)) = history {
                jobs.set(history);
            }
            gloo_timers::future::TimeoutFuture::new(5000).await;
        }
    });

    let sign_out = move |_| {
        if signing_out.get_untracked() {
            return;
        }
        signing_out.set(true);
        selected.set(None);
        playback_notice.set(None);
        let token = csrf.get_value();
        spawn_local(async move {
            match Request::post("/api/v1/auth/logout")
                .header("x-csrf-token", &token)
                .send()
                .await
            {
                Ok(response) if matches!(response.status(), 204 | 401) => session.set(None),
                _ => {
                    let _ = signing_out.try_update(|busy| *busy = false);
                    let _ = error.try_update(|error| {
                        *error = Some("Unable to sign out. Please retry.".into())
                    });
                }
            }
        });
    };
    let diagnose = move |_| {
        if requesting.get_untracked() {
            return;
        }
        let token = csrf.get_value();
        requesting.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                let response = Request::post("/api/v1/jobs/diagnostics")
                    .header("x-csrf-token", &token)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                decode::<DiagnosticJob>(response).await
            }
            .await;
            if requesting.try_update(|busy| *busy = false).is_some() {
                match result {
                    Ok(job) => jobs.update(|jobs| jobs.insert(0, job)),
                    Err(message) => error.set(Some(message)),
                }
            }
        });
    };

    view! {
        <div class="toolbar">
            <span class="subtle">"Signed in as " {username}</span>
            <button class="secondary" on:click=sign_out disabled=move || signing_out.get()>"Sign out"</button>
        </div>
        <p class="eyebrow">"Your movie collection"</p>
        <h1>"Movie night starts here."</h1>
        <p class="notice">
            "DVD playback is in preview, using H.264/AAC. Original ISOs and tracks are kept; sources are never
             automatically deleted. Audio-track selection, image subtitles and OCR captions are not available."
        </p>
        <For each={move || selected.get().into_iter().collect::<Vec<_>>()}
            key=|movie| movie.id.clone()
            children=move |movie| view! {
                <MoviePlayer movie=movie csrf=csrf selected=selected playback_notice=playback_notice/>
            }/>
        <Show when=move || selected.get().is_none() && !signing_out.get()>
        {move || playback_notice.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        <div class="toolbar">
            <h2>"Your library"</h2>
            <input aria-label="Search your library" placeholder="Search titles, descriptions and labels"
                type="search" bind:value=query/>
        </div>
        <div class="toolbar">
            <label>"Category "
                <select aria-label="Filter by category" bind:value=category>
                    <option value="">"All categories"</option>
                    <For each=move || library_label_names(movies.get(), true, category.get())
                        key=|name| name.clone()
                        children=|name| view! { <option value=name.clone()>{name.clone()}</option> }/>
                </select>
            </label>
            <label>"Tag "
                <select aria-label="Filter by tag" bind:value=tag>
                    <option value="">"All tags"</option>
                    <For each=move || library_label_names(movies.get(), false, tag.get())
                        key=|name| name.clone()
                        children=|name| view! { <option value=name.clone()>{name.clone()}</option> }/>
                </select>
            </label>
            <span class="subtle">"Search and selected filters must all match."</span>
        </div>
        <For each=move || editing_labels.get().into_iter()
            key=|movie| movie.id.clone()
            children=move |movie| view! {
                <MovieLabelsEditor movie=movie csrf=csrf movies=movies editing=editing_labels/>
            }/>
        {is_admin.then(|| view! { <TrashPanel csrf=csrf movies=movies/> })}
        <Show when=move || loading.get()>
            <p role="status" class="subtle">"Loading your library..."</p>
        </Show>
        <Show when=move || !loading.get()>
            <Show when=move || filtered.get().is_empty()>
                    <section class="empty">
                        <h2>{move || if movies.get().is_empty() { "A home for your DVD collection" } else { "No matching movies" }}</h2>
                        <p class="subtle">
                            {move || if movies.get().is_empty() {
                                "No movies yet. An administrator must review and publish a validated conversion before it appears here."
                            } else {
                                "Try another search or clear the filters."
                            }}
                        </p>
                    </section>
            </Show>
                    <div class="grid">
                        <For each=move || filtered.get()
                            key=|movie| (movie.id.clone(), movie.title.clone(), movie.year, movie.description.clone(), movie.metadata_expires_at, movie.poster_expires_at)
                            children=move |movie| {
                            let watch_label = format!("Watch {}", movie.title);
                            let chosen = movie.clone();
                            let label_id = movie.id.clone();
                            let edit_id = movie.id.clone();
                            let trash_id = movie.id.clone();
                            let poster_retry = RwSignal::new(0_u64);
                            view! {
                            <article class="movie">
                                <MovieArtwork movie=movie.clone() retry=poster_retry/>
                                <h2>{movie.title}</h2>
                                <small>{movie.year.map(|year| year.to_string())}</small>
                                <p>{movie.description}</p>
                                {move || movies.get().into_iter().find(|movie| movie.id == label_id).map(|movie|
                                    view! {
                                        {(!movie.categories.is_empty()).then(|| view! {
                                            <p class="subtle">"Categories: " {movie.categories.join(", ")}</p>
                                        })}
                                        {(!movie.tags.is_empty()).then(|| view! {
                                            <p class="subtle">"Tags: " {movie.tags.join(", ")}</p>
                                        })}
                                    }
                                )}
                                {is_admin.then(|| view! {
                                    <TrashButton id=trash_id csrf=csrf movies=movies editing=editing_labels/>
                                    <button class="secondary" disabled=move || editing_labels.get().is_some()
                                        on:click=move |_| editing_labels.set(
                                            movies.get_untracked().into_iter().find(|movie| movie.id == edit_id)
                                        )>"Edit labels"</button>
                                })}
                                <button on:click=move |_| {
                                    watch_movie.run(chosen.clone());
                                }
                                    disabled=move || signing_out.get()>{watch_label}</button>
                                {if is_admin && movie.tmdb_id.is_some() {
                                    Some(view! { <MetadataRefresh movie_id=movie.id expires=movie.metadata_expires_at csrf=csrf movies=movies poster_retry=poster_retry/> })
                                } else { None }}
                            </article>
                        }}/>
                    </div>
        </Show>
        </Show>
        {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        {is_admin.then(|| view! {
            <section class="panel">
                <div class="toolbar">
                    <div>
                        <h2>"Media readiness"</h2>
                        <p class="subtle">
                            {move || status.get().map(|system| {
                                let recent = system.worker_last_seen.is_some_and(|last_seen|
                                    (js_sys::Date::now() / 1000.0 - last_seen as f64).abs() < 30.0);
                                if recent { "Worker connected" } else { "Worker offline or not started" }
                            }).unwrap_or("Checking worker...")}
                        </p>
                    </div>
                    <button class="secondary" on:click=diagnose disabled=move || requesting.get()>
                        "Check again"
                    </button>
                </div>
                <For each={move || jobs.get().into_iter().take(1).collect::<Vec<_>>()}
                    key=|job| job.id.clone()
                    children=move |job| view! { <ReadinessCheck id=job.id jobs=jobs latest=true/> }/>
                <Show when=move || jobs.get().is_empty()>
                    <p class="subtle">"Not checked yet. Read-only checks never open or delete an ISO."</p>
                </Show>
                <details data-testid="previous-checks">
                    <summary>{move || format!("Previous checks ({})", jobs.get().len().saturating_sub(1))}</summary>
                    <For each={move || jobs.get().into_iter().skip(1).collect::<Vec<_>>()}
                        key=|job| job.id.clone()
                        children=move |job| view! { <ReadinessCheck id=job.id jobs=jobs latest=false/> }/>
                    <p class="subtle">"Showing up to 19 previous checks. All reports are kept."</p>
                </details>
            </section>
            <Show when=move || selected.get().is_none() && !signing_out.get()>
                <ImportsPanel csrf=csrf movies=movies watch_movie=watch_movie/>
            </Show>
        })}
    }
}

#[component]
fn TrashButton(
    id: String,
    csrf: StoredValue<String>,
    movies: RwSignal<Vec<Movie>>,
    editing: RwSignal<Option<Movie>>,
) -> impl IntoView {
    let scope = admin_scope();
    let id = StoredValue::new(id);
    let revision = expect_context::<LibraryRevision>().0;
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let remove = move |_| {
        if busy.get_untracked() || editing.get_untracked().is_some() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let result = admin_post::<bool>(
                scope,
                &format!("/api/v1/admin/movies/{}/trash", id.get_value()),
                &csrf.get_value(),
                &(),
            )
            .await;
            if busy.try_set(false).is_some() {
                return;
            }
            match result {
                Ok(_) => {
                    revision.update(|v| *v += 1);
                    movies.update(|values| values.retain(|m| m.id != id.get_value()));
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    view! {
        <button class="secondary" disabled=move || busy.get() || editing.get().is_some()
            on:click=move |_| confirming.set(true)>"Move to Trash"</button>
        <Show when=move || confirming.get()>
            <p>"Remove from the library? You can restore it for 30 days. After that, its movie files are permanently deleted. Source ISOs, logs and reports are kept."</p>
            <button disabled=move || busy.get() on:click=remove>"Confirm move to Trash"</button>
            <button class="secondary" disabled=move || busy.get() on:click=move |_| confirming.set(false)>"Cancel"</button>
        </Show>
        {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
    }
}

#[component]
fn TrashPanel(csrf: StoredValue<String>, movies: RwSignal<Vec<Movie>>) -> impl IntoView {
    let scope = admin_scope();
    let revision = expect_context::<LibraryRevision>().0;
    let entries = RwSignal::new(Vec::<wetube_core::TrashMovie>::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let mounted = Arc::new(AtomicBool::new(true));
    let cleanup = mounted.clone();
    on_cleanup(move || cleanup.store(false, Ordering::Relaxed));
    spawn_local(async move {
        while mounted.load(Ordering::Relaxed) {
            let before = revision.get_untracked();
            let result =
                admin_get::<Vec<wetube_core::TrashMovie>>(scope, "/api/v1/admin/trash").await;
            if !mounted.load(Ordering::Relaxed) {
                break;
            }
            match result {
                Ok(values) if before == revision.get_untracked() => entries.set(values),
                Ok(_) => {}
                Err(message) => error.set(Some(message)),
            }
            gloo_timers::future::TimeoutFuture::new(5000).await;
        }
    });
    view! {
        <details class="panel">
            <summary>"Trash"</summary>
            <p class="subtle">"Restore movies within 30 days. Movies awaiting deletion may remain here while playback finishes or cleanup needs attention."</p>
            <Show when=move || entries.get().is_empty()><p>"Trash is empty."</p></Show>
            <For each=move || entries.get() key=|entry| (entry.id.clone(),entry.purging,entry.error.clone())
                children=move |entry| {
                    let id = StoredValue::new(entry.id);
                    let expired = entry.purging || entry.purge_after as f64 <= js_sys::Date::now()/1000.0;
                    let restore = move |_| {
                        if busy.get_untracked() { return; }
                        busy.set(true);
                        error.set(None);
                        spawn_local(async move {
                            let result = admin_post::<bool>(scope, &format!("/api/v1/admin/movies/{}/restore",id.get_value()), &csrf.get_value(), &()).await;
                            if busy.try_set(false).is_some() { return; }
                            match result {
                                Ok(_) => {
                                    revision.update(|v| *v += 1);
                                    entries.update(|values| values.retain(|entry| entry.id != id.get_value()));
                                    let before = revision.get_untracked();
                                    if let Ok(library) = admin_get::<Library>(scope,"/api/v1/movies").await
                                        && before == revision.get_untracked() {
                                        movies.set(library.movies);
                                    }
                                },
                                Err(message) => error.set(Some(message)),
                            }
                        });
                    };
                    view! {
                        <article>
                            <h3>{entry.title}</h3>
                            <p>{if expired { format!("Recovery ended {}. Awaiting permanent deletion.",local_time(entry.purge_after)) }
                                else { format!("Restore before {}.",local_time(entry.purge_after)) }}</p>
                            {entry.error.map(|_| view! { <p role="alert" class="error">"Cleanup could not finish. Remaining files are kept for a safe retry. Check worker logs and storage access."</p> })}
                            <button disabled=move || expired || busy.get() on:click=restore>"Restore movie"</button>
                        </article>
                    }
                }/>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </details>
    }
}

#[component]
fn MovieArtwork(
    movie: Movie,
    #[prop(default = false)] icon: bool,
    #[prop(default = RwSignal::new(0_u64))] retry: RwSignal<u64>,
) -> impl IntoView {
    let failed = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let available = movie.tmdb_id.is_some() && movie.poster_expires_at.is_some();
    let source = format!(
        "/api/v1/movies/{}/poster?v={}",
        js_sys::encode_uri_component(&movie.id),
        movie.poster_expires_at.unwrap_or_default()
    );
    view! {
        <div class=if icon { "movie-artwork movie-icon" } else { "movie-artwork" }>
            <span class="generic-artwork" role="img" aria-label="No poster available"
                hidden=move || loaded.get() && !failed.get()>"▧"</span>
            <For each=move || if available { vec![retry.get()] } else { Vec::new() }
                key=|revision| *revision
                children=move |_| {
                    failed.set(false);
                    loaded.set(false);
                    view! {
                        <img src=source.clone() alt=format!("Poster for {}", movie.title)
                            loading="lazy" decoding="async" hidden=move || failed.get()
                            on:load=move |_| loaded.set(true)
                            on:error=move |_| failed.set(true)/>
                    }
                }/>
        </div>
    }
}

fn library_label_names(movies: Vec<Movie>, categories: bool, selected: String) -> Vec<String> {
    let mut names = movies
        .into_iter()
        .flat_map(|movie| {
            if categories {
                movie.categories
            } else {
                movie.tags
            }
        })
        .collect::<Vec<_>>();
    if !selected.is_empty() {
        names.push(selected);
    }
    names = names
        .into_iter()
        .map(|name| wetube_core::normalize_label_name(&name))
        .collect();
    names.sort_by_key(|name| wetube_core::label_key(name));
    names.dedup_by(|left, right| wetube_core::label_key(left) == wetube_core::label_key(right));
    names
}

fn append_label(text: RwSignal<String>, name: &str) {
    if name.is_empty() {
        return;
    }
    text.update(|text| {
        if !text
            .lines()
            .any(|line| wetube_core::label_key(line) == wetube_core::label_key(name))
        {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(name);
        }
    });
}

#[component]
fn LabelField(
    title: &'static str,
    text: RwSignal<String>,
    names: Signal<Vec<String>>,
    disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <label>{title}
            <textarea aria-label=title rows="3" bind:value=text disabled=move || disabled.get()/>
        </label>
        <select aria-label=format!("Reuse {title}") disabled=move || disabled.get()
            on:change=move |event| {
                append_label(text, &event_target_value(&event));
                event_target::<web_sys::HtmlSelectElement>(&event).set_value("");
            }>
            <option value="">"Choose an existing label…"</option>
            {move || names.get().into_iter().map(|name|
                view! { <option value=name.clone()>{name.clone()}</option> }).collect_view()}
        </select>
    }
}

#[component]
fn MovieLabelsEditor(
    movie: Movie,
    csrf: StoredValue<String>,
    movies: RwSignal<Vec<Movie>>,
    editing: RwSignal<Option<Movie>>,
) -> impl IntoView {
    let scope = admin_scope();
    let library_revision = expect_context::<LibraryRevision>().0;
    let id = StoredValue::new(movie.id);
    let labels_url = StoredValue::new(format!(
        "/api/v1/movies/{}/labels",
        js_sys::encode_uri_component(&id.get_value())
    ));
    let expected = RwSignal::new(None::<wetube_core::MovieLabels>);
    let latest = RwSignal::new(None::<wetube_core::MovieLabels>);
    let categories = RwSignal::new(String::new());
    let tags = RwSignal::new(String::new());
    let catalog = RwSignal::new(wetube_core::MovieLabels::default());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let suggestions = RwSignal::new(Vec::<String>::new());
    let approved = RwSignal::new(Vec::<String>::new());
    let fields_disabled = Signal::derive(move || busy.get() || expected.get().is_none());
    let load_latest = Callback::new(move |()| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        message.set(None);
        let initialize = expected.get_untracked().is_none();
        spawn_local(async move {
            let result =
                admin_get::<wetube_core::MovieLabels>(scope, &labels_url.get_value()).await;
            if busy.try_set(false).is_some() {
                return;
            }
            match result {
                Ok(labels) => {
                    if initialize {
                        categories.set(labels.categories.join("\n"));
                        tags.set(labels.tags.join("\n"));
                    } else {
                        latest.set(Some(labels.clone()));
                    }
                    expected.set(Some(labels));
                }
                Err(error) => message.set(Some(error)),
            }
        });
    });
    load_latest.run(());
    spawn_local(async move {
        if let Ok(labels) = admin_get::<wetube_core::MovieLabels>(scope, "/api/v1/labels").await {
            let _ = catalog.try_set(labels);
        }
    });
    let suggest = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        message.set(None);
        spawn_local(async move {
            let result = admin_post::<wetube_core::GenreSuggestions>(
                scope,
                &format!(
                    "/api/v1/admin/movies/{}/genre-suggestions",
                    js_sys::encode_uri_component(&id.get_value())
                ),
                &csrf.get_value(),
                &(),
            )
            .await;
            if busy.try_set(false).is_some() {
                return;
            }
            match result {
                Ok(result) => {
                    approved.set(Vec::new());
                    if result.categories.is_empty() {
                        message.set(Some("No genre suggestions found.".into()));
                    }
                    suggestions.set(result.categories);
                }
                Err(error) => message.set(Some(error)),
            }
        });
    };
    let save = move |_| {
        if busy.get_untracked() {
            return;
        }
        let Some(expected_labels) = expected.get_untracked() else {
            return;
        };
        busy.set(true);
        message.set(None);
        let mut category_names = categories
            .get_untracked()
            .lines()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        category_names.extend(approved.get_untracked());
        let request = wetube_core::UpdateMovieLabels {
            expected: expected_labels,
            labels: wetube_core::MovieLabels {
                categories: category_names,
                tags: tags
                    .get_untracked()
                    .lines()
                    .filter(|name| !name.trim().is_empty())
                    .map(str::to_owned)
                    .collect(),
            },
        };
        spawn_local(async move {
            let current_url = labels_url.get_value();
            let result = admin_post::<wetube_core::MovieLabels>(
                scope,
                &format!(
                    "/api/v1/admin/movies/{}/labels",
                    js_sys::encode_uri_component(&id.get_value())
                ),
                &csrf.get_value(),
                &request,
            )
            .await;
            match result {
                Ok(labels) => {
                    if busy.try_set(false).is_some() {
                        return;
                    }
                    library_revision.update(|revision| *revision += 1);
                    movies.update(|movies| {
                        if let Some(movie) =
                            movies.iter_mut().find(|movie| movie.id == id.get_value())
                        {
                            movie.categories = labels.categories;
                            movie.tags = labels.tags;
                        }
                    });
                    editing.set(None);
                }
                Err(error) => {
                    let current = admin_get::<wetube_core::MovieLabels>(scope, &current_url).await;
                    if busy.try_set(false).is_some() {
                        return;
                    }
                    if let Ok(labels) = current {
                        expected.set(Some(labels.clone()));
                        latest.set(Some(labels));
                    }
                    message.set(Some(error));
                }
            }
        });
    };
    view! {
        <section class="panel" aria-label="Edit movie labels">
            <h2>{format!("Labels for {}", movie.title)}</h2>
            <p class="subtle">"One label per line; up to 32 categories and 32 tags, 80 bytes each."</p>
            <LabelField title="Categories" text=categories names=Signal::derive(move || catalog.get().categories) disabled=fields_disabled/>
            <LabelField title="Tags" text=tags names=Signal::derive(move || catalog.get().tags) disabled=fields_disabled/>
            {move || latest.get().map(|labels| view! {
                <section aria-label="Latest saved labels">
                    <h3>"Latest saved labels"</h3>
                    <p>"Categories: " {if labels.categories.is_empty() { "None".into() } else { labels.categories.join(", ") }}</p>
                    <p>"Tags: " {if labels.tags.is_empty() { "None".into() } else { labels.tags.join(", ") }}</p>
                </section>
            })}
            {movie.tmdb_id.is_some().then(|| view! {
                <button class="secondary" disabled=move || fields_disabled.get() on:click=suggest>"Suggest TMDB genres"</button>
                <p class="subtle">"Choose suggestions to add as categories, then save. Existing labels are kept."</p>
            })}
            <For each=move || suggestions.get() key=|name| name.clone() children=move |name| {
                let choice = name.clone();
                let checked_name = name.clone();
                view! {
                    <label class="confirmation">
                        <input type="checkbox" disabled=move || busy.get()
                            prop:checked=move || approved.get().contains(&checked_name)
                            on:change=move |event| approved.update(|approved| {
                                approved.retain(|name| name != &choice);
                                if event_target_checked(&event) { approved.push(choice.clone()); }
                            })/>
                        {name}
                    </label>
                }
            }/>
            <div class="toolbar">
                <button disabled=move || fields_disabled.get() on:click=save>"Save labels"</button>
                <button class="secondary" disabled=move || busy.get() on:click=move |_| load_latest.run(())>"Reload latest labels"</button>
                <button class="secondary" disabled=move || busy.get() on:click=move |_| editing.set(None)>"Cancel"</button>
            </div>
            {move || message.get().map(|message| view! { <p role="alert">{message}</p> })}
        </section>
    }
}

#[component]
fn MetadataRefresh(
    movie_id: String,
    expires: Option<i64>,
    csrf: StoredValue<String>,
    movies: RwSignal<Vec<Movie>>,
    poster_retry: RwSignal<u64>,
) -> impl IntoView {
    let scope = admin_scope();
    let library_revision = expect_context::<LibraryRevision>().0;
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let url = StoredValue::new(format!(
        "/api/v1/admin/movies/{}/metadata/refresh",
        js_sys::encode_uri_component(&movie_id)
    ));
    let refresh = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        message.set(None);
        spawn_local(async move {
            let result =
                admin_post::<MetadataMatch>(scope, &url.get_value(), &csrf.get_value(), &()).await;
            if busy.try_update(|busy| *busy = false).is_none() {
                return;
            }
            match result {
                Ok(_) => {
                    poster_retry.update(|revision| *revision += 1);
                    let revision = library_revision.get_untracked();
                    match admin_get::<Library>(scope, "/api/v1/movies").await {
                        Ok(library) if library_revision.try_get_untracked() == Some(revision) => {
                            let _ = movies.try_set(library.movies);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = message.try_set(Some(error));
                        }
                    }
                }
                Err(error) => message.set(Some(error)),
            }
        });
    };
    view! {
        <p class="subtle">{format!("TMDB metadata expires: {}. Local playback remains available.", expires.map(local_time).unwrap_or_default())}</p>
        <button class="secondary" disabled=move || busy.get() on:click=refresh>"Refresh TMDB metadata and poster"</button>
        {move || message.get().map(|message| view! { <p role="alert">{message}</p> })}
    }
}

fn timestamp_date(seconds: i64) -> Option<js_sys::Date> {
    let date = js_sys::Date::new(&js_sys::wasm_bindgen::JsValue::from_f64(
        seconds as f64 * 1000.0,
    ));
    date.get_time().is_finite().then_some(date)
}

fn local_time(seconds: i64) -> String {
    thread_local! {
        static FORMATTER: js_sys::Intl::DateTimeFormat = {
            let options = js_sys::Object::new();
            for (key, value) in [
                ("year", "numeric"), ("month", "short"), ("day", "numeric"),
                ("hour", "numeric"), ("minute", "2-digit"), ("second", "2-digit"),
                ("timeZoneName", "longOffset"),
            ] {
                js_sys::Reflect::set(&options, &key.into(), &value.into())
                    .expect("date format option");
            }
            js_sys::Intl::DateTimeFormat::new(&js_sys::Array::new(), &options)
        };
    }
    let Some(date) = timestamp_date(seconds) else {
        return "Unknown time".into();
    };
    FORMATTER.with(|formatter| {
        formatter
            .format()
            .call1(&js_sys::wasm_bindgen::JsValue::UNDEFINED, &date)
            .ok()
            .and_then(|value| value.as_string())
            .unwrap_or_else(|| date.to_string().into())
    })
}

#[component]
fn ReadinessCheck(id: String, jobs: RwSignal<Vec<DiagnosticJob>>, latest: bool) -> impl IntoView {
    let id = StoredValue::new(id);
    let job = Signal::derive(move || jobs.get().into_iter().find(|job| job.id == id.get_value()));
    let label = move || {
        job.get().map(|job| match job.state {
            JobState::Queued => "Queued — waiting for worker",
            JobState::Running => "Checking media tools",
            JobState::Succeeded | JobState::Failed => {
                if job
                    .report
                    .as_ref()
                    .is_some_and(DiagnosticReport::required_tools_available)
                {
                    "Required media capabilities available"
                } else {
                    "Media capabilities need attention"
                }
            }
        })
    };
    let timestamp = move || {
        job.get()
            .map(|job| job.finished_at.unwrap_or(job.created_at))
    };
    view! {
        <div data-testid=if latest { "latest-readiness" } else { "previous-readiness" }>
            <p><strong>{label}</strong>" · "
                {move || if job.get().is_some_and(|job| job.finished_at.is_some()) { "Last checked: " } else { "Requested: " }}
                <time datetime=move || timestamp().and_then(timestamp_date).map(|date| String::from(date.to_iso_string()))>
                    {move || timestamp().map(local_time)}
                </time>
            </p>
            {move || job.get().and_then(|job| job.error).map(|message| view! { <p role="alert" class="error">{message}</p> })}
            <details>
                <summary>"Individual checks"</summary>
                <p class="subtle">"Checks tool availability, not conversion quality. No ISO is opened or deleted.
                    DVD discovery, FFmpeg, ffprobe and DVD-Video support are required; OCR is optional."</p>
                {move || job.get().and_then(|job| job.report).map(diagnostic_capabilities)}
            </details>
        </div>
    }
}

struct ProgressWriter {
    active: bool,
    resume_applied: bool,
    closing: bool,
    saving: bool,
    pending: Option<f64>,
    in_flight: Option<f64>,
    last_queued_ms: f64,
    controller: Option<web_sys::AbortController>,
    url: String,
    csrf: String,
    error: RwSignal<Option<String>>,
    navigation_error: RwSignal<Option<String>>,
    saved: RwSignal<Option<f64>>,
    selected: RwSignal<Option<Movie>>,
}

fn diagnostic_capabilities(report: DiagnosticReport) -> AnyView {
    let required_ready = report.required_tools_available();
    let scanner = report
        .tools
        .iter()
        .find(|tool| tool.name == "DVD discovery");
    let scan_status = match scanner {
        Some(tool) if tool.available => "DVD title discovery available.",
        Some(_) => {
            "DVD title discovery unavailable. Check the DVD library or fallback scanner below."
        }
        None => "DVD title discovery was not checked. Run a new check.",
    };
    let conversion_ready = ["ffmpeg", "ffprobe", "dvdvideo"].iter().all(|name| {
        report
            .tools
            .iter()
            .any(|tool| tool.name == *name && tool.available)
    });
    let tools = report
        .tools
        .into_iter()
        .map(|tool| {
            let capability = match tool.name.as_str() {
                "DVD discovery" => "Required for DVD title discovery",
                "lsdvd" => "Optional fallback for DVD title discovery",
                "ffmpeg" | "ffprobe" | "dvdvideo" => "Required for DVD inspection and conversion",
                "tesseract" | "English OCR" => "Optional OCR",
                _ => "Additional capability",
            };
            view! {
                <div class="tool" class:available=tool.available>
                    <strong>{tool.name}</strong>
                    <span>{capability}": "{if tool.available { "available" } else { "unavailable" }}
                        <br/>{tool.detail}</span>
                </div>
            }
        })
        .collect_view();
    view! {
        <p class="notice">{scan_status}</p>
        {required_ready.then(|| view! {
            <p class="subtle">"Required tools are available. OCR is optional."</p>
        })}
        <p class="subtle">{if conversion_ready {
            "Conversion tools are available; each conversion still needs validation and review."
        } else {
            "Conversion tools are missing or unchecked. Review FFmpeg, ffprobe and DVD-Video support below."
        }}</p>
        {tools}
    }.into_any()
}

fn queue_progress(writer: Rc<RefCell<ProgressWriter>>, position: f64, force: bool, close: bool) {
    {
        let mut state = writer.borrow_mut();
        if !state.active || state.closing {
            return;
        }
        if !position.is_finite() || position < 0.0 {
            if close {
                let selected = state.selected;
                let notice = state.navigation_error;
                drop(state);
                let _ = notice.try_set(Some(
                    "Could not save your resume position: the player position is unavailable."
                        .into(),
                ));
                let _ = selected.try_set(None);
            }
            return;
        }
        if close && !state.resume_applied {
            let selected = state.selected;
            drop(state);
            let _ = selected.try_set(None);
            return;
        }
        if !state.resume_applied {
            return;
        }
        let now = js_sys::Date::now();
        if !force && now - state.last_queued_ms < 5000.0 {
            return;
        }
        state.closing = close;
        state.last_queued_ms = now;
        if state.pending == Some(position)
            || (state.pending.is_none() && state.in_flight == Some(position))
        {
            return;
        }
        if !state.saving && state.saved.get_untracked() == Some(position) {
            let selected = state.selected;
            drop(state);
            if close {
                let _ = selected.try_set(None);
            }
            return;
        }
        // Only one PUT may be in flight; newer events replace the queued position.
        state.pending = Some(position);
        if state.saving {
            return;
        }
        state.saving = true;
    }
    spawn_local(async move {
        loop {
            let request = {
                let mut state = writer.borrow_mut();
                if !state.active {
                    return;
                }
                state.pending.take().map(|position| {
                    state.in_flight = Some(position);
                    (
                        position,
                        state.url.clone(),
                        state.csrf.clone(),
                        state
                            .controller
                            .as_ref()
                            .map(|controller| controller.signal()),
                    )
                })
            };
            let Some((position_seconds, url, csrf, signal)) = request else {
                let mut state = writer.borrow_mut();
                state.saving = false;
                let close = state.closing.then_some(state.selected);
                drop(state);
                if let Some(selected) = close {
                    let _ = selected.try_set(None);
                }
                return;
            };
            let result = async {
                let response = Request::put(&url)
                    .header("x-csrf-token", &csrf)
                    .abort_signal(signal.as_ref())
                    .json(&UpdatePlaybackProgress { position_seconds })
                    .map_err(|error| error.to_string())?
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                decode::<PlaybackProgress>(response).await
            }
            .await;
            let mut state = writer.borrow_mut();
            state.in_flight = None;
            if !state.active {
                return;
            }
            match result {
                Ok(progress) => {
                    let _ = state.saved.try_set(Some(progress.position_seconds));
                    let _ = state.error.try_set(None);
                }
                Err(message) => {
                    let _ = state
                        .error
                        .try_set(Some(format!("Could not save your position: {message}")));
                    if state.closing {
                        // Navigation must not depend on a successful save. The notice
                        // belongs to the library so player cleanup cannot erase it.
                        let selected = state.selected;
                        let notice = state.navigation_error;
                        state.saving = false;
                        state.pending = None;
                        drop(state);
                        let _ = notice.try_set(Some(format!(
                            "Could not save your resume position before returning to the library: {message}"
                        )));
                        let _ = selected.try_set(None);
                        return;
                    }
                }
            }
        }
    });
}

#[component]
fn MoviePlayer(
    movie: Movie,
    csrf: StoredValue<String>,
    selected: RwSignal<Option<Movie>>,
    playback_notice: RwSignal<Option<String>>,
) -> impl IntoView {
    let details = RwSignal::new(None::<MovieDetails>);
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(None::<String>);
    let media_error = RwSignal::new(None::<String>);
    let save_error = RwSignal::new(None::<String>);
    let saved = RwSignal::new(None::<f64>);
    let resume = RwSignal::new(0.0_f64);
    let restored = RwSignal::new(false);
    let video = NodeRef::<leptos::html::Video>::new();
    let id = js_sys::encode_uri_component(&movie.id)
        .as_string()
        .unwrap_or_default();
    let movie_url = format!("/api/v1/movies/{id}");
    let video_url = RwSignal::new(String::new());
    let writer = StoredValue::new_local(Rc::new(RefCell::new(ProgressWriter {
        active: true,
        resume_applied: false,
        closing: false,
        saving: false,
        pending: None,
        in_flight: None,
        last_queued_ms: js_sys::Date::now(),
        controller: web_sys::AbortController::new().ok(),
        url: format!("{movie_url}/progress"),
        csrf: csrf.get_value(),
        error: save_error,
        navigation_error: playback_notice,
        saved,
        selected,
    })));
    on_cleanup(move || {
        let writer = writer.get_value();
        let mut state = writer.borrow_mut();
        state.active = false;
        state.pending = None;
        if let Some(controller) = &state.controller {
            controller.abort();
        }
        drop(state);
        if let Some(element) = video.get_untracked() {
            let _ = element.pause();
            element.remove_attribute("src").ok();
            element.load();
        }
    });
    spawn_local(async move {
        let state = writer.get_value();
        let signal = state
            .borrow()
            .controller
            .as_ref()
            .map(|controller| controller.signal());
        let result = async {
            let Some(signal) = signal.as_ref() else {
                return Err(
                    "This browser cannot initialize cancellable playback requests.".to_owned(),
                );
            };
            let response = Request::get(&movie_url)
                .abort_signal(Some(signal))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let details = decode::<MovieDetails>(response).await?;
            let response = Request::get(&format!("{movie_url}/progress"))
                .abort_signal(Some(signal))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let progress = decode::<PlaybackProgress>(response).await?;
            let response = Request::post(&format!("{movie_url}/playback"))
                .header("x-csrf-token", &csrf.get_value())
                .abort_signal(Some(signal))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let lease = decode::<wetube_core::PlaybackLease>(response).await?;
            Ok((details, progress, lease))
        }
        .await;
        if !state.borrow().active {
            return;
        }
        loading.set(false);
        match result {
            Ok((movie, progress, lease)) => {
                video_url.set(format!("{movie_url}/video?playback={}", lease.id));
                state.borrow_mut().url = format!("{movie_url}/progress?playback={}", lease.id);
                resume.set(progress.position_seconds);
                saved.set(Some(progress.position_seconds));
                details.set(Some(movie));
                // Ranged media GETs cannot carry CSRF headers. The opaque lease is
                // bound to this login; only an actively playing element renews it.
                spawn_local(async move {
                    loop {
                        gloo_timers::future::TimeoutFuture::new(30_000).await;
                        if !state.borrow().active {
                            break;
                        }
                        if let Some(element) = video.get_untracked()
                            && !element.paused()
                            && !element.ended()
                        {
                            let result =
                                Request::post(&format!("{movie_url}/playback/{}", lease.id))
                                    .header("x-csrf-token", &csrf.get_value())
                                    .abort_signal(signal.as_ref())
                                    .send()
                                    .await;
                            if !state.borrow().active {
                                break;
                            }
                            if !matches!(result, Ok(response) if response.ok()) {
                                media_error.set(Some("Playback authorization ended. Return to the library to check availability or sign in again.".into()));
                                break;
                            }
                        }
                    }
                });
            }
            Err(message) => load_error.set(Some(format!("Unable to load this movie: {message}"))),
        }
    });

    let record = move |force| {
        let Some(state) = writer.try_get_value() else {
            return;
        };
        if !state.borrow().active {
            return;
        }
        if let Some(element) = video.get_untracked() {
            queue_progress(state, element.current_time(), force, false);
        }
    };
    let back = move |_| {
        let Some(state) = writer.try_get_value() else {
            return;
        };
        if let Some(element) = video.get_untracked() {
            // Queue before pausing: the resulting pause event must not supersede a close.
            queue_progress(state.clone(), element.current_time(), true, true);
            if state.borrow().active {
                let _ = element.pause();
            }
        } else {
            selected.set(None);
        }
    };
    let pane_label = format!("Watch {}", movie.title);
    let player_label = format!("Movie player: {}", movie.title);
    view! {
        <section class="panel player-pane" aria-label=pane_label>
            <button class="secondary" on:click=back>"Back to library"</button>
            <MovieArtwork movie=movie.clone() icon=true/>
            <h2>{movie.title}</h2>
            <small>{movie.year.map(|year| year.to_string())}</small>
            <p class="subtle">{movie.description}</p>
            {move || loading.get().then(|| view! { <p role="status">"Loading movie and your saved position..."</p> })}
            {move || load_error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
            <Show when=move || details.get().is_some()>
                <video node_ref=video data-testid="movie-player" controls preload="metadata"
                    src=move || video_url.get() aria-label=player_label.clone()
                    on:loadedmetadata=move |_| {
                        let Some(state) = writer.try_get_value() else { return; };
                        if !state.borrow().active || state.borrow().resume_applied {
                            return;
                        }
                        if let Some(element) = video.get_untracked() {
                            let duration = element.duration();
                            let position = resume.get_untracked();
                            let position = if duration.is_finite() && duration > 0.0
                                && position.is_finite() && position > 0.0 && position < duration {
                                position
                            } else { 0.0 };
                            element.set_current_time(position);
                            state.borrow_mut().resume_applied = true;
                            restored.set(true);
                        }
                    }
                    on:timeupdate=move |_| record(false)
                    on:pause=move |_| record(true)
                    on:seeked=move |_| record(true)
                    on:ended=move |_| record(true)
                    on:error=move |_| {
                        if writer.try_get_value().is_some_and(|state| state.borrow().active) {
                            media_error.set(Some("The movie could not be played. Check your browser's H.264/AAC support or ask your administrator to check the video file.".into()));
                        }
                    }>
                    "Your browser does not support native video playback."
                </video>
                <p class="subtle">
                    {move || details.get().map(|movie| approximate_duration(Some(movie.duration_seconds)))}
                </p>
                <p role="status" class="subtle">
                    {move || if restored.get() {
                        format!("Saved position: {:.1} seconds. Pausing and seeking save your progress.",
                            saved.get().unwrap_or(0.0))
                    } else {
                        "Waiting for video metadata to restore your position...".to_owned()
                    }}
                </p>
            </Show>
            {move || media_error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
            {move || save_error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </section>
    }
}

#[component]
fn ImportsPanel(
    csrf: StoredValue<String>,
    movies: RwSignal<Vec<Movie>>,
    watch_movie: Callback<Movie>,
) -> impl IntoView {
    let incoming = RwSignal::new(None::<IncomingFiles>);
    let jobs = RwSignal::new(Vec::<ProbeJob>::new());
    let scans = RwSignal::new(Vec::<ScanJob>::new());
    let conversions = RwSignal::new(Vec::<ConversionJob>::new());
    let selected = RwSignal::new(String::new());
    let copy_complete = RwSignal::new(false);
    let refreshing = RwSignal::new(false);
    let requesting = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let main_entries = RwSignal::new(Vec::<ImportEntry>::new());
    let history_entries = RwSignal::new(Vec::<ImportEntry>::new());
    let main_cursor = RwSignal::new(None::<String>);
    let main_next = RwSignal::new(None::<String>);
    let main_previous = RwSignal::new(Vec::<Option<String>>::new());
    let history_count = RwSignal::new(0_u64);
    let history_open = RwSignal::new(false);
    let history_generation = RwSignal::new(0_u64);
    let history_next = RwSignal::new(None::<String>);
    let history_busy = RwSignal::new(false);
    let history_error = RwSignal::new(None::<String>);
    let jobs_error = RwSignal::new(None::<String>);
    let revision = RwSignal::new(0_u64);
    let scope = admin_scope();
    let mounted = Arc::new(AtomicBool::new(true));
    let cleanup = mounted.clone();
    on_cleanup(move || cleanup.store(false, Ordering::Relaxed));

    let refresh = move || {
        if refreshing.get_untracked() {
            return;
        }
        refreshing.set(true);
        selected.set(String::new());
        copy_complete.set(false);
        error.set(None);
        spawn_local(async move {
            let result = admin_get::<IncomingFiles>(scope, "/api/v1/admin/incoming").await;
            if refreshing.try_update(|busy| *busy = false).is_none() {
                return;
            }
            match result {
                Ok(files) => incoming.set(Some(files)),
                Err(message) => error.set(Some(message)),
            }
        });
    };
    refresh();
    Effect::new(move |_| {
        let main = main_entries.get();
        let history = history_entries.get();
        let mut scan_list = Vec::new();
        let mut probe_list = Vec::new();
        let mut conversion_list = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in main.into_iter().chain(history) {
            if !seen.insert(import_key(&entry)) {
                continue;
            }
            match entry.item {
                ImportJob::Scan(job) => scan_list.push(job),
                ImportJob::Probe(job) => probe_list.push(job),
                ImportJob::Conversion(job) => conversion_list.push(job),
            }
        }
        scans.set(scan_list);
        jobs.set(probe_list);
        conversions.set(conversion_list);
    });
    let load_history = move |reset: bool| {
        let before = if reset {
            None
        } else {
            history_next.get_untracked()
        };
        let cursor = before
            .map(|cursor| format!("&before={cursor}"))
            .unwrap_or_default();
        let generation = history_generation.get_untracked() + 1;
        history_generation.set(generation);
        history_busy.set(true);
        spawn_local(async move {
            let result = admin_get::<ImportPage>(
                scope,
                &format!("/api/v1/admin/imports?history=true&limit=10{cursor}"),
            )
            .await;
            if history_generation.try_get_untracked() != Some(generation) {
                return;
            }
            history_busy.set(false);
            match result {
                Ok(page) => {
                    remove_import_overlaps(main_entries, &page.entries);
                    history_next.set(page.next);
                    if reset {
                        history_entries.set(page.entries);
                    } else {
                        history_entries.update(|entries| {
                            for entry in page.entries {
                                if !entries
                                    .iter()
                                    .any(|existing| import_key(existing) == import_key(&entry))
                                {
                                    entries.push(entry);
                                }
                            }
                        });
                    }
                    history_error.set(None);
                }
                Err(message) => history_error.set(Some(message)),
            }
        });
    };
    Effect::new(move |_| {
        if !history_open.get() {
            return;
        }
        history_count.get();
        revision.get();
        load_history(true);
    });
    spawn_local(async move {
        while mounted.load(Ordering::Relaxed) {
            let before = main_cursor.get_untracked();
            let cursor = before
                .as_ref()
                .map(|cursor| format!("&before={cursor}"))
                .unwrap_or_default();
            let result =
                admin_get::<ImportPage>(scope, &format!("/api/v1/admin/imports?limit=50{cursor}"))
                    .await;
            if !mounted.load(Ordering::Relaxed) {
                break;
            }
            if before == main_cursor.get_untracked() {
                match result {
                    Ok(page) => {
                        if before.is_some() && page.entries.is_empty() {
                            main_cursor
                                .set(main_previous.get_untracked().last().cloned().flatten());
                            main_previous.update(|previous| {
                                previous.pop();
                            });
                        }
                        remove_import_overlaps(history_entries, &page.entries);
                        main_entries.set(page.entries);
                        main_next.set(page.next);
                        if history_count.get_untracked() != page.history_count {
                            history_count.set(page.history_count);
                        }
                        jobs_error.set(None);
                    }
                    Err(message) => jobs_error.set(Some(message)),
                }
            }
            gloo_timers::future::TimeoutFuture::new(1000).await;
        }
    });

    let submit = move |event: web_sys::SubmitEvent| {
        event.prevent_default();
        if requesting.get_untracked() || refreshing.get_untracked() {
            return;
        }
        let file = incoming.get_untracked().and_then(|list| {
            list.files
                .into_iter()
                .find(|file| file.name == selected.get_untracked())
        });
        let Some(file) = file else {
            error.set(Some("Select a completed ISO copy first.".into()));
            return;
        };
        if !copy_complete.get_untracked() {
            error.set(Some(
                "Confirm the ISO copy is finished before scanning.".into(),
            ));
            return;
        }
        let request = ScanRequest {
            name: file.name,
            identity: file.identity,
            copy_complete: copy_complete.get_untracked(),
        };
        let token = csrf.get_value();
        requesting.set(true);
        error.set(None);
        spawn_local(async move {
            let result =
                admin_post::<ScanJob>(scope, "/api/v1/admin/scans", &token, &request).await;
            if requesting.try_update(|busy| *busy = false).is_none() {
                return;
            }
            copy_complete.set(false);
            match result {
                Ok(job) => {
                    main_cursor.set(None);
                    main_previous.set(Vec::new());
                    scans.update(|scans| {
                        scans.retain(|existing| existing.id != job.id);
                        scans.insert(0, job);
                    });
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };

    view! {
        <section class="panel">
            <div class="toolbar">
                <h2>"Scan an incoming DVD copy"</h2>
                <button class="secondary" on:click=move |_| refresh()
                    disabled=move || refreshing.get() || requesting.get()>
                    "Refresh incoming files"
                </button>
            </div>
            <p class="notice">
                "Scan a verified private copy to find DVD titles and angles, then choose a title to inspect.
                 Scanning does not convert, publish or delete anything."
            </p>
            {move || incoming.get().map(|list| if !list.configured {
                view! { <p class="subtle">"DVD scanning is disabled. Ask your administrator to configure incoming copies and private working storage."</p> }.into_any()
            } else if list.files.is_empty() {
                view! { <p class="subtle">"No readable, non-empty ISO files in the incoming folder. Finish copying, then refresh."</p> }.into_any()
            } else {
                view! { <p class="subtle">"Use finished incoming copies only, never the original archive. Scans wait for a connected, configured worker."</p> }.into_any()
            })}
            <form on:submit=submit>
                <label for="incoming-iso">"Completed ISO copy"</label>
                <select id="incoming-iso" required
                    prop:value=move || selected.get()
                    on:change=move |event| {
                        selected.set(event_target_value(&event));
                        copy_complete.set(false);
                    }
                    disabled=move || refreshing.get() || requesting.get()>
                    <option value="">"Select an ISO..."</option>
                    <For each=move || incoming.get().map(|list| list.files).unwrap_or_default()
                        key=|file| (file.name.clone(), file.identity.clone())
                        children=|file| {
                            let label = format!("{} ({:.2} GiB)", file.name, file.size_bytes as f64 / 1_073_741_824.0);
                            view! { <option value=file.name>{label}</option> }
                        }/>
                </select>
                <label class="confirmation">
                    <input type="checkbox" required prop:checked=move || copy_complete.get()
                        on:change=move |event| copy_complete.set(event_target_checked(&event))/>
                    "I confirm this copy is finished and its SMB writers are closed."
                </label>
                <p class="subtle">"Confirmation cannot verify that writers are closed and does not authorize source deletion."</p>
                <button type="submit" disabled=move || requesting.get() || refreshing.get()
                    || !copy_complete.get() || selected.get().is_empty()
                    || scans.get().iter().any(|job| matches!(job.state, ScanState::Queued | ScanState::Staging | ScanState::Scanning))>
                    {move || if requesting.get() { "Queueing scan..." } else { "Scan DVD" }}
                </button>
            </form>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
            <h2>"Imports needing attention"</h2>
            <p class="subtle">"Expand an import to review it. Errors stay here until archived.
                Archiving only moves a job to your History; inputs, reports and media are kept."</p>
            {move || jobs_error.get().map(|message| view! { <p role="alert" class="error">"Could not refresh imports: "{message}</p> })}
            <For each=move || main_entries.get() key=import_key
                children=move |entry| view! {
                    <ImportRow entry_key=import_key(&entry) entries=main_entries history=false
                        csrf=csrf scans=scans probes=jobs conversions=conversions movies=movies
                        watch_movie=watch_movie revision=revision/>
                }/>
            <div class="toolbar">
                <Show when=move || !main_previous.get().is_empty()>
                    <button class="secondary" on:click=move |_| {
                        main_cursor.set(main_previous.get_untracked().last().cloned().flatten());
                        main_previous.update(|previous| { previous.pop(); });
                    }>"Previous imports"</button>
                </Show>
                <Show when=move || main_next.get().is_some()>
                    <button class="secondary" on:click=move |_| {
                        main_previous.update(|previous| previous.push(main_cursor.get_untracked()));
                        main_cursor.set(main_next.get_untracked());
                        main_next.set(None);
                    }>"More imports needing attention"</button>
                </Show>
            </div>
            <p class="subtle">"Every attempt uses working storage, even if it fails or stops. An operator must review storage use; nothing is automatically deleted."</p>
            <details data-testid="import-history" on:toggle=move |event| {
                use js_sys::wasm_bindgen::JsCast;
                let open = event.target().and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                    .is_some_and(|target| target.has_attribute("open"));
                history_open.set(open);
            }>
                <summary>{move || format!("History ({})", history_count.get())}</summary>
                <p class="subtle">"Completed steps and archived jobs. Restoring a job returns it to your attention list without running it again."</p>
                <For each=move || history_entries.get() key=import_key
                    children=move |entry| view! {
                        <ImportRow entry_key=import_key(&entry) entries=history_entries history=true
                            csrf=csrf scans=scans probes=jobs conversions=conversions movies=movies
                            watch_movie=watch_movie revision=revision/>
                    }/>
                {move || history_error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
                <Show when=move || history_busy.get()><p role="status">"Loading history..."</p></Show>
                <Show when=move || history_next.get().is_some()>
                    <button class="secondary" disabled=move || history_busy.get()
                        on:click=move |_| load_history(false)>"Load older"</button>
                </Show>
                <Show when=move || history_error.get().is_some()>
                    <button class="secondary" disabled=move || history_busy.get()
                        on:click=move |_| load_history(true)>"Retry history"</button>
                </Show>
            </details>
        </section>
    }
}

fn import_key(entry: &ImportEntry) -> (String, String) {
    let (kind, id) = match &entry.item {
        ImportJob::Scan(job) => ("scan", &job.id),
        ImportJob::Probe(job) => ("probe", &job.id),
        ImportJob::Conversion(job) => ("conversion", &job.id),
    };
    (kind.into(), id.clone())
}

fn remove_import_overlaps(entries: RwSignal<Vec<ImportEntry>>, incoming: &[ImportEntry]) {
    let keys = incoming
        .iter()
        .map(import_key)
        .collect::<std::collections::HashSet<_>>();
    if entries
        .get_untracked()
        .iter()
        .any(|entry| keys.contains(&import_key(entry)))
    {
        entries.update(|entries| entries.retain(|entry| !keys.contains(&import_key(entry))));
    }
}

fn import_active(entry: &ImportEntry) -> bool {
    match &entry.item {
        ImportJob::Scan(job) => matches!(
            job.state,
            ScanState::Queued | ScanState::Staging | ScanState::Scanning
        ),
        ImportJob::Probe(job) => matches!(
            job.state,
            ProbeState::Queued | ProbeState::Staging | ProbeState::Probing
        ),
        ImportJob::Conversion(job) => matches!(
            job.state,
            ConversionState::Queued
                | ConversionState::Converting
                | ConversionState::QueuedPublication
                | ConversionState::Publishing
        ),
    }
}

fn import_label(entry: &ImportEntry) -> String {
    let (label, state, created) = match &entry.item {
        ImportJob::Scan(job) => (
            format!("DVD scan · {}", job.name),
            match job.state {
                ScanState::NeedsReview if entry.can_archive => "Inspection started — scan retained",
                ScanState::NeedsReview => "Needs review — choose a title",
                ScanState::Failed => "Failed — inputs retained",
                _ => "Active",
            },
            job.created_at,
        ),
        ImportJob::Probe(job) => (
            format!(
                "Inspection · {} · title {} / angle {}",
                job.name, job.title, job.angle
            ),
            match job.state {
                ProbeState::NeedsReview if entry.can_archive => {
                    "Conversion started — inspection retained"
                }
                ProbeState::NeedsReview => "Needs review — choose conversion treatment",
                ProbeState::Failed => "Failed — inputs retained",
                _ => "Active",
            },
            job.created_at,
        ),
        ImportJob::Conversion(job) => (
            format!("Conversion · {}", job.id),
            match job.state {
                ConversionState::NeedsReview => "Needs review — publication decision",
                ConversionState::Published => "Published",
                ConversionState::Failed => "Failed — inputs retained",
                _ => "Active",
            },
            job.created_at,
        ),
    };
    format!("{label} · {state} · {}", local_time(created))
}

#[component]
fn ImportRow(
    entry_key: (String, String),
    entries: RwSignal<Vec<ImportEntry>>,
    history: bool,
    csrf: StoredValue<String>,
    scans: RwSignal<Vec<ScanJob>>,
    probes: RwSignal<Vec<ProbeJob>>,
    conversions: RwSignal<Vec<ConversionJob>>,
    movies: RwSignal<Vec<Movie>>,
    watch_movie: Callback<Movie>,
    revision: RwSignal<u64>,
) -> impl IntoView {
    let key = StoredValue::new(entry_key);
    let entry = Signal::derive(move || {
        entries
            .get()
            .into_iter()
            .find(|entry| import_key(entry) == key.get_value())
    });
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let scope = admin_scope();
    let archive = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let (kind, id) = key.get_value();
        spawn_local(async move {
            let result = admin_post::<ArchiveImport>(
                scope,
                &format!("/api/v1/admin/imports/{kind}/{id}/archive"),
                &csrf.get_value(),
                &ArchiveImport { archived: !history },
            )
            .await;
            if busy.try_update(|busy| *busy = false).is_none() {
                return;
            }
            match result {
                Ok(_) => {
                    entries.update(|entries| {
                        entries.retain(|entry| import_key(entry) != key.get_value())
                    });
                    revision.update(|revision| *revision += 1);
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    let body = move || {
        let (kind, id) = key.get_value();
        match kind.as_str() {
            "scan" => view! { <ScanSelection id=id csrf=csrf scans=scans probes=probes/> }.into_any(),
            "probe" => view! { <ProbeReview id=id csrf=csrf probes=probes conversions=conversions/> }.into_any(),
            _ => view! { <ConversionReview id=id csrf=csrf jobs=conversions movies=movies watch_movie=watch_movie/> }.into_any(),
        }
    };
    view! {
        <div class="import-row" data-testid=if history { "history-row" } else { "attention-row" }
            data-job-id=key.get_value().1>
            <Show when=move || !history && entry.get().is_some_and(|entry| import_active(&entry))
                fallback=move || view! {
                    <details class="import-details">
                        <summary>{move || entry.get().map(|entry| import_label(&entry))}</summary>
                        {body()}
                    </details>
                }>
                {body()}
            </Show>
            {move || entry.get().and_then(|entry| match entry.item {
                ImportJob::Scan(job) => job.error,
                ImportJob::Probe(job) => job.error,
                ImportJob::Conversion(job) => job.error,
            }).map(|message| view! { <p role="alert" class="error">{message}</p> })}
            <Show when=move || entry.get().is_some_and(|entry| entry.can_archive)>
                <button class="secondary" disabled=move || busy.get() on:click=archive>
                    {if history { "Restore to attention" } else { "Archive to History" }}
                </button>
            </Show>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </div>
    }
}

#[component]
fn ProbeReview(
    id: String,
    csrf: StoredValue<String>,
    probes: RwSignal<Vec<ProbeJob>>,
    conversions: RwSignal<Vec<ConversionJob>>,
) -> impl IntoView {
    let id = StoredValue::new(id);
    view! {
        <For each={move || probes.get().into_iter().filter(|job| job.id == id.get_value()).collect::<Vec<_>>()}
            key=|job| (job.id.clone(), job.state)
            children=move |job| {
                let job_id = StoredValue::new(job.id.clone());
                let label = match job.state {
                    ProbeState::Queued => "Queued",
                    ProbeState::Staging => "Copying and verifying private input",
                    ProbeState::Probing => "Inspecting the selected title",
                    ProbeState::NeedsReview => "Needs review - not published",
                    ProbeState::Failed => "Failed - inputs retained",
                };
                view! {
                    <article class="job" data-testid="probe-job" data-job-id=job.id.clone()>
                        <h3 class="inspection-source">"Source: "{job.name.clone()}</h3>
                        <p class="inspection-state">{label}</p>
                        {move || probes.get().into_iter().find(|job| job.id == job_id.get_value()).map(|job| view! {
                            <MediaJobProgress observation_key=format!("inspect:{}", job.id) workflow=MediaWorkflow::Inspect progress=job.progress
                                state=match job.state {
                                    ProbeState::Queued => MediaProgressState::Queued,
                                    ProbeState::NeedsReview => MediaProgressState::Complete,
                                    ProbeState::Failed => MediaProgressState::Failed,
                                    _ => MediaProgressState::Active,
                                }/>
                        })}
                        {inspection_summary(&job)}
                        {(job.state == ProbeState::NeedsReview).then(|| view! {
                            <ProbeConversion probe_id=job.id.clone() csrf=csrf conversions=conversions/>
                        })}
                        {job.attempt_id.map(|attempt| view! { <p class="attempt">"Attempt: "{attempt}</p> })}
                        {job.error.map(|message| view! { <p class="error">{message}</p> })}
                        {job.report.map(|report| view! {
                            <details><summary>"Inspection details (JSON)"</summary><pre>{format!("{report:#}")}</pre></details>
                        })}
                    </article>
                }
            }/>
    }
}

fn scan_title_label(title: &wetube_core::DvdTitle) -> String {
    let duration = if title.duration_seconds.is_finite() && title.duration_seconds >= 0.0 {
        let seconds = title.duration_seconds.round() as u64;
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        "duration unavailable".into()
    };
    format!(
        "Title {} — {} · {} chapters{}",
        title.title,
        duration,
        title.chapter_count,
        if title.main_feature_candidate {
            " · longest-title candidate"
        } else {
            ""
        },
    )
}

#[component]
fn ScanSelection(
    id: String,
    csrf: StoredValue<String>,
    scans: RwSignal<Vec<ScanJob>>,
    probes: RwSignal<Vec<ProbeJob>>,
) -> impl IntoView {
    let id = StoredValue::new(id);
    let scan = Signal::derive(move || {
        scans
            .get()
            .into_iter()
            .find(|scan| scan.id == id.get_value())
    });
    let title = RwSignal::new(String::new());
    let angle = RwSignal::new(String::new());
    let selected_title = Signal::derive(move || {
        let title = title.get().parse::<u8>().ok()?;
        scan.get()?
            .report?
            .titles
            .into_iter()
            .find(|candidate| candidate.title == title)
    });
    let busy = RwSignal::new(false);
    let queued = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let scope = admin_scope();
    let ready = move || {
        scan.get()
            .is_some_and(|scan| scan.state == ScanState::NeedsReview)
    };
    let valid_selection = move || {
        selected_title.get().is_some_and(|title| {
            angle
                .get()
                .parse::<u8>()
                .ok()
                .is_some_and(|angle| title.angles.contains(&angle))
        })
    };
    let submit = move |event: web_sys::SubmitEvent| {
        event.prevent_default();
        if busy.get_untracked() || queued.get_untracked().is_some() || !ready() {
            return;
        }
        let (Some(selected), Ok(angle)) = (
            selected_title.get_untracked(),
            angle.get_untracked().parse::<u8>(),
        ) else {
            error.set(Some(
                "Choose a scanned title and one of its available angles.".into(),
            ));
            return;
        };
        if !selected.angles.contains(&angle) {
            error.set(Some(
                "This angle is not available for the scanned title.".into(),
            ));
            return;
        }
        let request = InspectRequest {
            title: selected.title,
            angle,
        };
        let url = format!(
            "/api/v1/admin/scans/{}/inspect",
            js_sys::encode_uri_component(&id.get_value())
                .as_string()
                .unwrap_or_default()
        );
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result = admin_post::<ProbeJob>(scope, &url, &csrf.get_value(), &request).await;
            if busy.try_update(|busy| *busy = false).is_none() {
                return;
            }
            match result {
                Ok(job) => {
                    queued.set(Some(job.id.clone()));
                    probes.update(|probes| {
                        probes.retain(|existing| existing.id != job.id);
                        probes.insert(0, job);
                    });
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    view! {
        <article class="job scan-job" data-testid="scan-job" data-job-id=id.get_value()>
            {move || scan.get().map(|scan| {
                let label = match scan.state {
                    ScanState::Queued => "Scan queued — waiting for worker",
                    ScanState::Staging => "Copying and verifying the ISO",
                    ScanState::Scanning => "Finding DVD titles and angles",
                    ScanState::NeedsReview => "Scan complete — select a title for inspection",
                    ScanState::Failed => "Scan failed — ISO, reports and logs retained",
                };
                view! {
                    <h3 class="inspection-source">"Source: "{scan.name}</h3>
                    <p class="inspection-state" role="status">{label}</p>
                    <MediaJobProgress observation_key=format!("scan:{}", scan.id) workflow=MediaWorkflow::Scan progress=scan.progress
                        state=match scan.state {
                            ScanState::Queued => MediaProgressState::Queued,
                            ScanState::NeedsReview => MediaProgressState::Complete,
                            ScanState::Failed => MediaProgressState::Failed,
                            _ => MediaProgressState::Active,
                        }/>
                    <p class="attempt">"Scan job: "{scan.id}</p>
                    {scan.attempt_id.map(|attempt| view! { <p class="attempt">"Attempt: "{attempt}</p> })}
                    {scan.error.map(|message| view! { <p role="alert" class="error">{message}</p> })}
                }
            })}
            <Show when=ready>
                <p class="field-help">"DVD titles are numbered programs: features, trailers or extras.
                    Neither title 1 nor the longest title is always the main feature. Review before choosing."</p>
                <Show when=move || scan.get().and_then(|scan| scan.report).is_none_or(|report| report.titles.is_empty())>
                    <p class="error">"No usable title inventory is available. Inspection is disabled; review the scan error or retry scanning."</p>
                </Show>
                <form on:submit=submit>
                    <label>
                        "DVD title"
                        <select required prop:value=move || title.get()
                            disabled=move || busy.get()
                            on:change=move |event| {
                                title.set(event_target_value(&event));
                                let angles = selected_title.get_untracked().map(|title| title.angles).unwrap_or_default();
                                angle.set(if angles.len() == 1 { angles[0].to_string() } else { String::new() });
                                queued.set(None);
                                error.set(None);
                            }>
                            <option value="">"Choose a scanned title..."</option>
                            <For each=move || scan.get().and_then(|scan| scan.report).map(|report| report.titles).unwrap_or_default()
                                key=|title| title.title
                                children=|title| {
                                    let label = scan_title_label(&title);
                                    view! { <option value=title.title.to_string()>{label}</option> }
                                }/>
                        </select>
                    </label>
                    {move || selected_title.get().map(|title| {
                        let languages = |languages: Vec<String>| if languages.is_empty() {
                            "not reported".to_owned()
                        } else { languages.join(", ") };
                        view! {
                            <p class="field-help">{format!(
                                "{} audio tracks ({}) · {} subtitle tracks ({})",
                                title.audio_count, languages(title.audio_languages),
                                title.subtitle_count, languages(title.subtitle_languages),
                            )}</p>
                            {(title.angles.len() == 1).then(|| view! {
                                <p class="field-help">{format!("Only available angle: {}", title.angles[0])}</p>
                            })}
                            {title.angles.is_empty().then(|| view! {
                                <p class="error">"No valid angles were reported for this title. Inspection is disabled."</p>
                            })}
                        }
                    })}
                    <Show when=move || selected_title.get().is_some_and(|title| title.angles.len() > 1)>
                        <label>
                            "DVD angle"
                            <select required prop:value=move || angle.get() disabled=move || busy.get()
                                on:change=move |event| {
                                    angle.set(event_target_value(&event));
                                    queued.set(None);
                                    error.set(None);
                                }>
                                <option value="">"Choose an available angle..."</option>
                                <For each=move || selected_title.get().map(|title| title.angles).unwrap_or_default()
                                    key=|angle| *angle
                                    children=|angle| view! { <option value=angle.to_string()>{format!("Angle {angle}")}</option> }/>
                            </select>
                        </label>
                        <p class="field-help">"Angles are alternate camera views on supported discs. Normally choose angle 1 when it is available."</p>
                    </Show>
                    <p class="subtle">"Inspection uses the verified private copy; no new NAS copy is needed."</p>
                    <button type="submit" disabled=move || busy.get() || queued.get().is_some() || !valid_selection()>
                        {move || if busy.get() { "Queueing inspection..." } else { "Inspect selected title" }}
                    </button>
                </form>
                {move || queued.get().map(|id| view! { <p role="status" class="subtle">"Inspection queued: "{id}". Review its report below before conversion."</p> })}
            </Show>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </article>
    }
}

fn update_conversion(jobs: RwSignal<Vec<ConversionJob>>, job: ConversionJob) {
    jobs.update(|jobs| {
        if let Some(existing) = jobs.iter_mut().find(|existing| existing.id == job.id) {
            *existing = job;
        } else {
            jobs.insert(0, job);
        }
    });
}

#[component]
fn ComparisonPreview(probe_id: String, csrf: StoredValue<String>) -> impl IntoView {
    let path = StoredValue::new(format!("/api/v1/admin/probes/{probe_id}/comparison"));
    let job = RwSignal::new(None::<ComparisonJob>);
    let loading = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let scope = admin_scope();
    let refresh = move || {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        spawn_local(async move {
            let result = admin_get::<Option<ComparisonJob>>(scope, &path.get_value()).await;
            if loading.try_update(|loading| *loading = false).is_none() {
                return;
            }
            match result {
                Ok(current) => {
                    job.set(current);
                    loaded.set(true);
                    error.set(None);
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    refresh();
    let timer = gloo_timers::callback::Interval::new(2000, move || {
        if job
            .get_untracked()
            .is_some_and(|job| matches!(job.state, JobState::Queued | JobState::Running))
        {
            refresh();
        }
    });
    let timer = StoredValue::new_local(Some(timer));
    on_cleanup(move || {
        let _ = timer.try_update_value(|timer| timer.take());
    });
    let submit = move |_| {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        error.set(None);
        spawn_local(async move {
            let result =
                admin_post::<ComparisonJob>(scope, &path.get_value(), &csrf.get_value(), &()).await;
            if loading.try_update(|loading| *loading = false).is_none() {
                return;
            }
            match result {
                Ok(current) => job.set(Some(current)),
                Err(message) => {
                    error.set(Some(message));
                    refresh();
                }
            }
        });
    };
    view! {
        <section class="comparison-preview" data-testid="comparison-preview">
            <h4>"Compare a midpoint sample"</h4>
            <p class="subtle">"Compare both treatments using the same 10-second midpoint sample, or less for short titles.
                Uses the verified private copy. Sample selection uses approximate DVD timing, not motion detection.
                These H.264/AAC previews are not lossless, do not validate the full title or disc, and do not authorize publication."</p>
            <div class="probe-options">
                <button type="button" on:click=submit disabled=move || !loaded.get() || loading.get()
                    || job.get().is_some_and(|job| job.state != JobState::Failed)>
                    "Generate Preserve vs Yadif preview"
                </button>
                <button type="button" on:click=move |_| refresh() disabled=move || loading.get()>
                    "Refresh comparison status"
                </button>
            </div>
            {move || job.get().map(|job| {
                let ready = job.state == JobState::Succeeded;
                let timing = job.report.as_ref().map(|report| format!(
                    "Approximate sample: {:.2}s from the start, {:.2}s long, using the inspected title and angle.",
                    report["requested_start_seconds"].as_f64().unwrap_or(0.0),
                    report["requested_duration_seconds"].as_f64().unwrap_or(0.0)));
                view! {
                    <MediaJobProgress observation_key=format!("comparison:{}", job.id) workflow=MediaWorkflow::Comparison progress=job.progress state=match job.state {
                        JobState::Queued => MediaProgressState::Queued,
                        JobState::Running => MediaProgressState::Active,
                        JobState::Succeeded => MediaProgressState::Complete,
                        JobState::Failed => MediaProgressState::Failed,
                    }/>
                    <p role="status">{match job.state {
                        JobState::Queued => "Comparison queued — waiting for worker.",
                        JobState::Running => "Generating and decoding both comparison clips...",
                        JobState::Succeeded => "Both clips decoded and ready. Nothing was published.",
                        JobState::Failed => "Comparison failed. Partial files kept; nothing published.",
                    }}</p>
                    {job.error.map(|message| view! { <p role="alert" class="error">{message}</p> })}
                    {timing.map(|text| view! { <p class="subtle">{text}</p> })}
                    {ready.then(|| {
                        let preserve = format!("/api/v1/admin/comparisons/{}/preserve/video",job.id);
                        let yadif = format!("/api/v1/admin/comparisons/{}/yadif/video",job.id);
                        view! {
                            <div>
                                <h5>"Preserve — no deinterlacing (not lossless)"</h5>
                                <video controls playsinline preload="metadata" aria-label="Preserve comparison" src=preserve style="width:100%;max-width:640px"/>
                                <h5>"Yadif — deinterlaced"</h5>
                                <video controls playsinline preload="metadata" aria-label="Yadif comparison" src=yadif style="width:100%;max-width:640px"/>
                            </div>
                        }
                    })}
                }
            })}
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </section>
    }
}

#[component]
fn ProbeConversion(
    probe_id: String,
    csrf: StoredValue<String>,
    conversions: RwSignal<Vec<ConversionJob>>,
) -> impl IntoView {
    let probe_id = StoredValue::new(probe_id);
    let treatment = RwSignal::new(String::new());
    let reviewed = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let submitted = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let scope = admin_scope();
    let existing = move || {
        conversions.get().iter().any(|job| {
            job.probe_job_id == probe_id.get_value() && job.state != ConversionState::Failed
        })
    };
    let submit = move |event: web_sys::SubmitEvent| {
        event.prevent_default();
        if busy.get_untracked() || submitted.get_untracked() || existing() {
            return;
        }
        let video_treatment = match treatment.get_untracked().as_str() {
            "preserve" => VideoTreatment::Preserve,
            "yadif" => VideoTreatment::Yadif,
            _ => {
                error.set(Some("Select a video treatment explicitly.".into()));
                return;
            }
        };
        if !reviewed.get_untracked() {
            error.set(Some(
                "Review the inspection, warnings and selected video treatment first.".into(),
            ));
            return;
        }
        busy.set(true);
        error.set(None);
        let request = ConversionRequest {
            probe_job_id: probe_id.get_value(),
            video_treatment,
        };
        spawn_local(async move {
            let result = admin_post::<ConversionJob>(
                scope,
                "/api/v1/admin/conversions",
                &csrf.get_value(),
                &request,
            )
            .await;
            if busy.try_update(|busy| *busy = false).is_none() {
                return;
            }
            match result {
                Ok(job) => {
                    submitted.set(true);
                    update_conversion(conversions, job);
                }
                Err(message) => error.set(Some(message)),
            }
        });
    };
    view! {
        <div class="conversion-controls">
            <ComparisonPreview probe_id=probe_id.get_value() csrf=csrf/>
            <h4>"Review before conversion"</h4>
            <p class="notice">"Review the program, duration, field order and warnings.
                Both treatments are preview profiles, not approved for production. Original tracks are kept;
                image subtitles and OCR captions are not available for playback."</p>
            <form on:submit=submit>
                <label>
                    "Video treatment"
                    <select required prop:value=move || treatment.get()
                        disabled=move || busy.get() || submitted.get() || existing()
                        on:change=move |event| {
                            treatment.set(event_target_value(&event));
                            reviewed.set(false);
                        }>
                        <option value="">"Choose a treatment..."</option>
                        <option value="preserve">"Preserve — no deinterlacing filter"</option>
                        <option value="yadif">"Yadif — apply deinterlacing"</option>
                    </select>
                </label>
                <p class="field-help">"Preserve re-encodes video without deinterlacing; it is not lossless.
                    Use Yadif only if deinterlacing is appropriate for this title."</p>
                <label class="confirmation">
                    <input type="checkbox" required prop:checked=move || reviewed.get()
                        disabled=move || busy.get() || submitted.get() || existing()
                        on:change=move |event| reviewed.set(event_target_checked(&event))/>
                    "I reviewed this inspection and warnings, and chose this preview treatment."
                </label>
                <button type="submit" disabled=move || busy.get() || submitted.get() || existing()
                    || !reviewed.get() || treatment.get().is_empty()>
                    {move || if busy.get() { "Queueing conversion..." } else { "Convert reviewed title" }}
                </button>
            </form>
            {move || (submitted.get() || existing()).then(|| view! {
                <p role="status" class="subtle">"Conversion queued or already exists. Review it below; publication requires separate approval."</p>
            })}
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </div>
    }
}

fn validated_hash(job: &ConversionJob) -> Option<String> {
    job.movie_sha256
        .as_ref()
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .cloned()
}

fn publication_metadata(
    title: String,
    year: String,
    description: String,
) -> Result<(String, Option<i32>, String), String> {
    let title = title.trim().to_owned();
    if title.is_empty() || title.len() > 512 {
        return Err("Enter a nonempty movie title of at most 512 UTF-8 bytes.".into());
    }
    let year = if year.trim().is_empty() {
        None
    } else {
        Some(
            year.trim()
                .parse::<i32>()
                .map_err(|_| "Enter a valid year, or leave it blank.".to_owned())?,
        )
    };
    if year.is_some_and(|year| !(1888..=2100).contains(&year)) {
        return Err("Year must be between 1888 and 2100, or left blank.".into());
    }
    let description = description.trim().to_owned();
    if description.len() > 16384 {
        return Err("Description must be no longer than 16384 UTF-8 bytes.".into());
    }
    Ok((title, year, description))
}

fn browser_clock() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(f64::NAN, |performance| performance.now() / 1000.0)
}

fn metadata_is_fresh(received_at: f64) -> bool {
    (0.0..86400.0).contains(&(browser_clock() - received_at))
}

#[derive(Clone)]
struct MetadataSelection {
    source: TmdbSource,
    received_at: f64,
    label: String,
}

#[component]
fn MetadataLookup(
    csrf: StoredValue<String>,
    title: RwSignal<String>,
    year: RwSignal<String>,
    description: RwSignal<String>,
    source: RwSignal<Option<MetadataSelection>>,
    confirmed: RwSignal<bool>,
    disabled: Signal<bool>,
    clock: RwSignal<f64>,
) -> impl IntoView {
    let scope = admin_scope();
    let configured = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let search_year = RwSignal::new(String::new());
    let searching = RwSignal::new(false);
    let results = RwSignal::new(Vec::<(MetadataMatch, f64)>::new());
    let show_results = RwSignal::new(true);
    let message = RwSignal::new(None::<String>);
    let search_input = NodeRef::<leptos::html::Input>::new();
    let cached_matches_available = move || {
        let _ = clock.get();
        results.with(|results| {
            results
                .iter()
                .any(|(_, received_at)| metadata_is_fresh(*received_at))
        })
    };
    Effect::new(move |_| {
        if source
            .get()
            .is_some_and(|selection| !metadata_is_fresh(selection.received_at))
        {
            results.set(Vec::new());
        }
    });
    spawn_local(async move {
        let result = admin_get::<MetadataStatus>(scope, "/api/v1/admin/metadata").await;
        if configured.try_get_untracked().is_none() {
            return;
        }
        match result {
            Ok(status) => {
                configured.set(status.configured);
                if !status.configured {
                    message.set(Some(
                        "TMDB lookup is disabled; manual entry is available.".into(),
                    ));
                }
            }
            Err(_) => message.set(Some(
                "Metadata lookup is unavailable; manual entry is available.".into(),
            )),
        }
    });
    let search = move |event: web_sys::SubmitEvent| {
        event.prevent_default();
        if disabled.get_untracked() || searching.get_untracked() || !configured.get_untracked() {
            return;
        }
        let year = search_year.get_untracked();
        let year = if year.trim().is_empty() {
            None
        } else {
            match year.trim().parse::<i32>() {
                Ok(year) if (1888..=2100).contains(&year) => Some(year),
                _ => {
                    message.set(Some("Enter a year from 1888 to 2100.".into()));
                    return;
                }
            }
        };
        let query = query.get_untracked().trim().to_owned();
        if query.is_empty() || query.len() > 256 || query.chars().any(char::is_control) {
            message.set(Some("Enter a search title (1–256 UTF-8 bytes).".into()));
            return;
        }
        searching.set(true);
        show_results.set(true);
        results.set(Vec::new());
        message.set(None);
        spawn_local(async move {
            let result = admin_post::<MetadataResults>(
                scope,
                "/api/v1/admin/metadata/search",
                &csrf.get_value(),
                &MetadataSearch { query, year },
            )
            .await;
            if searching
                .try_update(|searching| *searching = false)
                .is_none()
            {
                return;
            }
            match result {
                Ok(response) => {
                    if response.movies.is_empty() {
                        message.set(Some(
                            "No matches; try another search or enter metadata manually.".into(),
                        ));
                    }
                    // Browser time is only a local elapsed-time hint. The server
                    // independently validates the unchanged fetched_at at publish.
                    let received_at = browser_clock();
                    results.set(
                        response
                            .movies
                            .into_iter()
                            .map(|movie| (movie, received_at))
                            .collect(),
                    );
                }
                Err(error) => message.set(Some(error)),
            }
        });
    };
    view! {
        <section class="metadata-lookup">
            <h4>"Optional TMDB lookup"</h4>
            <p class="subtle">"TMDB receives only the search title and optional year you enter, not filenames, ISOs or media.
                Choose a match, then review or edit it. Cached metadata expires after 150 days; administrators refresh it manually."</p>
            <form on:submit=search>
                <label>"Search TMDB title"<input node_ref=search_input maxlength="256" required bind:value=query
                    disabled=move || disabled.get() || searching.get() || !configured.get()/></label>
                <label>"Search TMDB year (optional)"<input type="number" min="1888" max="2100" bind:value=search_year
                    disabled=move || disabled.get() || searching.get() || !configured.get()/></label>
                <button type="submit" disabled=move || disabled.get() || searching.get() || !configured.get()>
                    {move || if searching.get() { "Searching TMDB..." } else { "Search TMDB" }}
                </button>
            </form>
            {move || message.get().map(|message| view! { <p role="status">{message}</p> })}
            {move || source.get().map(|selection| view! {
                <section aria-label="Selected TMDB match">
                    <strong>{selection.label}</strong>
                    <p class="subtle">"Source: TMDB"</p>
                    <Show when=move || !show_results.get() && cached_matches_available()>
                        <button type="button" class="secondary" disabled=move || disabled.get() || searching.get()
                            on:click=move |_| {
                                if disabled.get_untracked() || searching.get_untracked() { return; }
                                if !results.with_untracked(|results| results.iter().any(|(_, received_at)| metadata_is_fresh(*received_at))) {
                                    clock.set(browser_clock());
                                    return;
                                }
                                show_results.set(true);
                            }>"Change match"</button>
                    </Show>
                    <Show when=move || !cached_matches_available()>
                        <p class="subtle">"Search again for other matches. Your edits are kept until you select a match."</p>
                        <button type="button" class="secondary" disabled=move || disabled.get() || searching.get() || !configured.get()
                            on:click=move |_| {
                                if disabled.get_untracked() || searching.get_untracked() || !configured.get_untracked() { return; }
                                if let Some(input) = search_input.get_untracked() {
                                    let _ = input.focus();
                                }
                            }>"Search again"</button>
                    </Show>
                </section>
            })}
            <Show when=move || show_results.get()>
            {move || results.get().into_iter().map(|(matched, received_at)| {
                let label = format!("{} ({})", matched.title, matched.year.map(|year| year.to_string()).unwrap_or_else(|| "year unknown".into()));
                view! {
                    <article>
                        <strong>{label.clone()}</strong>
                        <p>{matched.description.clone()}</p>
                        <button type="button" disabled=move || disabled.get() || searching.get()
                            on:click=move |_| {
                                if disabled.get_untracked() || searching.get_untracked() { return; }
                                if !metadata_is_fresh(received_at) {
                                    message.set(Some("Search result expired; search again.".into()));
                                    return;
                                }
                                title.set(matched.title.clone());
                                year.set(matched.year.map(|year| year.to_string()).unwrap_or_default());
                                description.set(matched.description.clone());
                                source.set(Some(MetadataSelection {
                                    source: TmdbSource { id: matched.id, fetched_at: matched.fetched_at },
                                    received_at,
                                    label: label.clone(),
                                }));
                                confirmed.set(false);
                                show_results.set(false);
                            }>"Use this match"</button>
                    </article>
                }
            }).collect_view()}
            </Show>
            <Show when=move || source.get().is_some()>
                <p class="subtle">"Selected TMDB metadata remains editable. Confirm publication separately within 24 hours."</p>
                <button type="button" class="secondary" disabled=move || disabled.get()
                    on:click=move |_| {
                        if disabled.get_untracked() { return; }
                        source.set(None);
                        title.set(String::new());
                        year.set(String::new());
                        description.set(String::new());
                        confirmed.set(false);
                        show_results.set(true);
                    }>"Clear TMDB selection for manual entry"</button>
            </Show>
        </section>
    }
}

#[component]
fn ConversionReview(
    id: String,
    csrf: StoredValue<String>,
    jobs: RwSignal<Vec<ConversionJob>>,
    movies: RwSignal<Vec<Movie>>,
    watch_movie: Callback<Movie>,
) -> impl IntoView {
    let library_revision = expect_context::<LibraryRevision>().0;
    let id = StoredValue::new(id);
    let job = Signal::derive(move || jobs.get().into_iter().find(|job| job.id == id.get_value()));
    let title = RwSignal::new(String::new());
    let year = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());
    let confirmed = RwSignal::new(false);
    let tmdb = RwSignal::new(None::<MetadataSelection>);
    let clock = RwSignal::new(browser_clock());
    let timer = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(
        30_000,
        move || {
            let _ = clock.try_set(browser_clock());
        },
    )));
    on_cleanup(move || {
        let _ = timer.try_update_value(|timer| timer.take());
    });
    let fresh = move || {
        let _ = clock.get();
        tmdb.get()
            .is_none_or(|selection| metadata_is_fresh(selection.received_at))
    };
    let busy = RwSignal::new(false);
    let submitted = RwSignal::new(false);
    let watching = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let scope = admin_scope();
    let ready = move || {
        job.get().is_some_and(|job| {
            job.state == ConversionState::NeedsReview && validated_hash(&job).is_some()
        })
    };
    let can_publish = move || {
        ready()
            && confirmed.get()
            && !busy.get()
            && !submitted.get()
            && publication_metadata(title.get(), year.get(), description.get()).is_ok()
            && fresh()
    };
    Effect::new(move |previous: Option<Option<String>>| {
        let hash = job.get().and_then(|job| validated_hash(&job));
        if previous.as_ref() != Some(&hash) {
            confirmed.set(false);
        }
        hash
    });
    let publish = move |event: web_sys::SubmitEvent| {
        event.prevent_default();
        if busy.get_untracked() || submitted.get_untracked() {
            return;
        }
        let Some(current) = job.get_untracked() else {
            return;
        };
        let Some(movie_sha256) = validated_hash(&current) else {
            error.set(Some(
                "This conversion has no validated checksum and cannot be published.".into(),
            ));
            return;
        };
        if current.state != ConversionState::NeedsReview || !confirmed.get_untracked() {
            error.set(Some(
                "Review a validated conversion and deliberately confirm publication first.".into(),
            ));
            return;
        }
        if tmdb
            .get_untracked()
            .is_some_and(|selection| !metadata_is_fresh(selection.received_at))
        {
            confirmed.set(false);
            error.set(Some(
                "TMDB selection expired; search again or clear it for manual entry.".into(),
            ));
            return;
        }
        let (title, year, description) = match publication_metadata(
            title.get_untracked(),
            year.get_untracked(),
            description.get_untracked(),
        ) {
            Ok(metadata) => metadata,
            Err(message) => {
                error.set(Some(message));
                return;
            }
        };
        let request = PublishRequest {
            title,
            year,
            description,
            movie_sha256,
            confirm: true,
            tmdb: tmdb.get_untracked().map(|selection| selection.source),
        };
        let url = format!(
            "/api/v1/admin/conversions/{}/publish",
            js_sys::encode_uri_component(&current.id)
                .as_string()
                .unwrap_or_default()
        );
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result =
                admin_post::<ConversionJob>(scope, &url, &csrf.get_value(), &request).await;
            if busy.try_update(|busy| *busy = false).is_none() {
                return;
            }
            match result {
                Ok(job) => {
                    submitted.set(true);
                    confirmed.set(false);
                    update_conversion(jobs, job);
                }
                Err(message) => {
                    if message == wetube_core::TMDB_SELECTION_EXPIRED {
                        confirmed.set(false);
                        tmdb.update(|selection| {
                            if let Some(selection) = selection {
                                selection.received_at = f64::NEG_INFINITY;
                            }
                        });
                    }
                    error.set(Some(message));
                }
            }
        });
    };
    let watch = move |_| {
        if watching.get_untracked() {
            return;
        }
        let Some(movie_id) = job
            .get_untracked()
            .filter(|job| job.state == ConversionState::Published)
            .and_then(|job| job.movie_id)
        else {
            return;
        };
        watching.set(true);
        error.set(None);
        spawn_local(async move {
            let revision = library_revision.get_untracked();
            let result = admin_get::<Library>(scope, "/api/v1/movies").await;
            if watching.try_update(|watching| *watching = false).is_none() {
                return;
            }
            match result {
                Ok(library) => {
                    let chosen = library
                        .movies
                        .iter()
                        .find(|movie| movie.id == movie_id)
                        .cloned();
                    if library_revision.get_untracked() == revision {
                        movies.set(library.movies);
                    }
                    if let Some(chosen) = chosen {
                        watch_movie.run(chosen);
                    } else {
                        error.set(Some("Published movie is not visible in the library yet. Refresh status and retry.".into()));
                    }
                }
                Err(message) => error.set(Some(format!(
                    "Could not load the published movie: {message}"
                ))),
            }
        });
    };
    let refreshed = RwSignal::new(false);
    Effect::new(move |_| {
        let published = job
            .get()
            .is_some_and(|job| job.state == ConversionState::Published);
        if published && !refreshed.get_untracked() {
            refreshed.set(true);
            spawn_local(async move {
                let revision = library_revision.get_untracked();
                let result = admin_get::<Library>(scope, "/api/v1/movies").await;
                if scope.try_get_value().is_none() {
                    return;
                }
                match result {
                    Ok(library) if library_revision.get_untracked() == revision => {
                        movies.set(library.movies)
                    }
                    Ok(_) => {}
                    Err(message) => error.set(Some(format!(
                        "Published, but the library refresh failed: {message}"
                    ))),
                }
            });
        }
    });
    view! {
        <article class="job conversion-job" data-testid="conversion-job" data-job-id=id.get_value()>
            <h3>"Conversion "{id.get_value()}</h3>
            {move || job.get().map(|job| {
                let missing_hash = job.state == ConversionState::NeedsReview && validated_hash(&job).is_none();
                let state = match job.state {
                    ConversionState::Queued => "Queued for conversion",
                    ConversionState::Converting => "Converting — this can take a long time",
                    ConversionState::NeedsReview => "Conversion needs review — not published",
                    ConversionState::QueuedPublication => "Publication queued — waiting for worker",
                    ConversionState::Publishing => "Publishing to the local library",
                    ConversionState::Published => "Published to the local library",
                    ConversionState::Failed => "Failed — ISO, original tracks, reports and logs retained",
                };
                view! {
                    <p class="inspection-state" role="status">{state}</p>
                    <MediaJobProgress observation_key=format!("conversion:{}", job.id) workflow=MediaWorkflow::Conversion progress=job.progress
                        state=match job.state {
                            ConversionState::Queued => MediaProgressState::Queued,
                            ConversionState::NeedsReview => MediaProgressState::Review,
                            ConversionState::QueuedPublication => MediaProgressState::QueuedPublication,
                            ConversionState::Published => MediaProgressState::Complete,
                            ConversionState::Failed => MediaProgressState::Failed,
                            _ => MediaProgressState::Active,
                        }/>
                    <p class="subtle">"Inspection job: "{job.probe_job_id}
                        " · Video treatment: "{match job.video_treatment {
                            VideoTreatment::Preserve => "Preserve (no deinterlacing filter)",
                            VideoTreatment::Yadif => "Yadif (deinterlacing)",
                        }}</p>
                    {job.phase.map(|phase| view! { <p>"Phase: "{phase}</p> })}
                    {job.attempt_id.map(|attempt| view! { <p class="attempt">"Attempt: "{attempt}</p> })}
                    {job.error.map(|message| view! { <p role="alert" class="error">{message}</p> })}
                    {missing_hash.then(|| view! {
                        <p class="error">"Validated checksum is unavailable. Publication is disabled."</p>
                    })}
                }
            })}
            <Show when=move || job.get().is_some_and(|job| job.report.is_some())>
                <details>
                    <summary>"Validation report and warnings (JSON)"</summary>
                    <pre>{move || job.get().and_then(|job| job.report).map(|report| format!("{report:#}"))}</pre>
                </details>
            </Show>
            <Show when=ready>
                <div class="publication-review">
                    <h4>"Review metadata and publish"</h4>
                    <p class="notice">"Review the validation report and warnings. Publication is tied to this conversion's
                        validated checksum, which is checked again before publishing.
                        This adds preview playback to your local library, not production approval.
                        Original tracks are kept; image subtitles and OCR captions are unavailable."</p>
                    <MetadataLookup csrf=csrf title=title year=year description=description
                        source=tmdb confirmed=confirmed clock=clock
                        disabled=Signal::derive(move || busy.get() || submitted.get() || !ready())/>
                    <Show when=move || !fresh()>
                        <p role="alert">"TMDB selection expired; search again or clear it for manual entry."</p>
                    </Show>
                    <form on:submit=publish>
                        <label>"Movie title"<input required maxlength="512" bind:value=title
                            on:input=move |_| confirmed.set(false)
                            disabled=move || busy.get() || submitted.get()/></label>
                        <label>"Year (optional)"<input type="number" min="1888" max="2100" bind:value=year
                            on:input=move |_| confirmed.set(false)
                            disabled=move || busy.get() || submitted.get()/></label>
                        <label>"Description"<textarea maxlength="16384" rows="3" bind:value=description
                            on:input=move |_| confirmed.set(false)
                            disabled=move || busy.get() || submitted.get()/></label>
                        {move || publication_metadata(title.get(), year.get(), description.get()).err()
                            .map(|message| view! { <p class="field-help">{message}</p> })}
                        <label class="confirmation">
                            <input type="checkbox" required prop:checked=move || confirmed.get()
                                disabled=move || busy.get() || submitted.get()
                                on:change=move |event| confirmed.set(event_target_checked(&event))/>
                            "I reviewed the validated conversion, warnings and metadata, and confirm publishing this movie to the local library."
                        </label>
                        <button type="submit" disabled=move || !can_publish()>
                            {move || if busy.get() { "Queueing publication..." } else { "Publish to local library" }}
                        </button>
                    </form>
                </div>
            </Show>
            <Show when=move || job.get().is_some_and(|job| job.state == ConversionState::Published)>
                <button on:click=watch disabled=move || watching.get()>
                    {move || if watching.get() { "Loading library..." } else { "Watch published movie" }}
                </button>
            </Show>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
        </article>
    }
}

fn approximate_duration(seconds: Option<f64>) -> String {
    match seconds.filter(|seconds| seconds.is_finite() && *seconds >= 0.0) {
        Some(seconds) => format!("About {:.0} min ({seconds:.1} s)", (seconds / 60.0).round()),
        None => "Not reported".to_owned(),
    }
}

fn inspection_summary(job: &ProbeJob) -> AnyView {
    let report = job.report.as_ref();
    let checksum = report
        .and_then(|report| report["sha256"].as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Not reported")
        .to_owned();
    let size = report
        .and_then(|report| report["size_bytes"].as_u64())
        .map(|bytes| format!("{:.2} GiB ({bytes} bytes)", bytes as f64 / 1_073_741_824.0))
        .unwrap_or_else(|| "Not reported".to_owned());
    let inspection = report
        .and_then(|report| report.get("inspection"))
        .filter(|inspection| inspection.is_object());
    let metadata = inspection
        .and_then(|inspection| inspection.get("metadata"))
        .filter(|metadata| metadata.is_object());
    let seconds = metadata.and_then(|metadata| {
        let duration = &metadata["format"]["duration"];
        duration
            .as_f64()
            .or_else(|| duration.as_str().and_then(|value| value.parse().ok()))
    });
    let chapters = metadata
        .and_then(|metadata| metadata["chapters"].as_array())
        .map(|chapters| chapters.len().to_string())
        .unwrap_or_else(|| "Not reported".to_owned());
    let streams = metadata.and_then(|metadata| metadata["streams"].as_array());
    let track_groups = [
        ("video", "Video"),
        ("audio", "Audio tracks"),
        ("subtitle", "Subtitle tracks"),
    ]
    .into_iter()
    .map(|(kind, heading)| {
        let tracks = streams
            .into_iter()
            .flatten()
            .filter(|stream| stream["codec_type"].as_str() == Some(kind))
            .enumerate()
            .map(|(position, stream)| {
                let codec = stream["codec_name"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("Codec not reported");
                let label = stream["index"]
                    .as_u64()
                    .map(|index| format!("Stream {index}"))
                    .unwrap_or_else(|| format!("Track {}", position + 1));
                let detail = if kind == "video" {
                    let resolution = match (
                        stream["width"].as_u64().filter(|width| *width > 0),
                        stream["height"].as_u64().filter(|height| *height > 0),
                    ) {
                        (Some(width), Some(height)) => format!("{width} × {height}"),
                        _ => "Resolution not reported".to_owned(),
                    };
                    let field_order = match stream["field_order"].as_str() {
                        Some("progressive") => "Progressive",
                        Some("tt") => "Top coded / top displayed first",
                        Some("bb") => "Bottom coded / bottom displayed first",
                        Some("tb") => "Top coded / bottom displayed first",
                        Some("bt") => "Bottom coded / top displayed first",
                        _ => "Not reported",
                    };
                    format!("{codec} · {resolution} · Field order: {field_order}")
                } else {
                    let language = stream["tags"]["language"]
                        .as_str()
                        .filter(|value| !value.trim().is_empty() && *value != "und")
                        .unwrap_or("Not reported");
                    if kind == "audio" {
                        let channels = stream["channels"]
                            .as_u64()
                            .filter(|channels| *channels > 0)
                            .map(|channels| channels.to_string())
                            .unwrap_or_else(|| "Not reported".to_owned());
                        format!("{codec} · Language: {language} · Channels: {channels}")
                    } else {
                        format!("{codec} · Language: {language}")
                    }
                };
                view! { <li><strong>{label}</strong><span>{detail}</span></li> }
            })
            .collect::<Vec<_>>();
        let empty = if streams.is_none() {
            "Track metadata not available."
        } else {
            "No tracks of this type reported."
        };
        view! {
            <div class="inspection-tracks">
                <h4>{heading}</h4>
                {if tracks.is_empty() {
                    view! { <p class="subtle">{empty}</p> }.into_any()
                } else {
                    view! { <ul>{tracks}</ul> }.into_any()
                }}
            </div>
        }
    })
    .collect_view();
    let warnings = inspection.and_then(|inspection| inspection["warnings"].as_array());
    let warning_items = warnings
        .into_iter()
        .flatten()
        .filter_map(|warning| warning.as_str())
        .filter(|warning| !warning.trim().is_empty())
        .map(|warning| view! { <li>{warning.to_owned()}</li> })
        .collect::<Vec<_>>();
    let warning_fallback = if warnings.is_some_and(|warnings| warnings.is_empty()) {
        "No inspection warnings reported."
    } else {
        "Inspection warnings not available; consult the raw report if present."
    };
    view! {
        <div class="inspection-summary">
            <dl class="inspection-facts">
                <div><dt>"Selected title / angle"</dt><dd>{format!("{} / {}", job.title, job.angle)}</dd></div>
                <div><dt>"Staged size"</dt><dd>{size}</dd></div>
                <div class="inspection-checksum"><dt>"Staged SHA-256"</dt><dd><code>{checksum}</code></dd></div>
            </dl>
            <h4>"Metadata inspection only"</h4>
            <p class="subtle">"Describes only the selected title and angle. This does not validate conversion or playback, or publish anything."</p>
            {metadata.is_none().then(|| view! {
                <p class="subtle">"DVD metadata is not available for this attempt. Staging details, if present, do not mean inspection succeeded."</p>
            })}
            <dl class="inspection-facts">
                <div><dt>"Approximate duration"</dt><dd>{approximate_duration(seconds)}</dd></div>
                <div><dt>"Chapter count"</dt><dd>{chapters}</dd></div>
            </dl>
            <p class="subtle">"Duration and chapter timing are approximate."</p>
            <div class="inspection-track-grid">{track_groups}</div>
            <div class="inspection-warnings">
                <h4>"Inspection warnings"</h4>
                {if warning_items.is_empty() {
                    view! { <p class="subtle">{warning_fallback}</p> }.into_any()
                } else {
                    view! { <ul>{warning_items}</ul> }.into_any()
                }}
            </div>
        </div>
    }
    .into_any()
}
