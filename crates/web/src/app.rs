use gloo_net::http::{Request, Response};
use leptos::{prelude::*, task::spawn_local};
use serde::de::DeserializeOwned;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use wetube_core::{
    ApiError, DiagnosticJob, JobState, Library, LoginRequest, Movie, Role, Session, SystemStatus,
};

async fn decode<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    if !response.ok() {
        return Err(response
            .json::<ApiError>()
            .await
            .map(|error| error.error)
            .unwrap_or_else(|_| format!("Request failed ({})", response.status())));
    }
    response.json().await.map_err(|error| error.to_string())
}

async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let response = Request::get(path)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    decode(response).await
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

#[component]
fn LibraryPage(current: Session, session: RwSignal<Option<Session>>) -> impl IntoView {
    let is_admin = current.user.role == Role::Admin;
    let username = current.user.username.clone();
    let csrf = StoredValue::new(current.csrf_token);
    let movies = RwSignal::new(Vec::<Movie>::new());
    let status = RwSignal::new(None::<SystemStatus>);
    let jobs = RwSignal::new(Vec::<DiagnosticJob>::new());
    let error = RwSignal::new(None::<String>);
    let query = RwSignal::new(String::new());
    let requesting = RwSignal::new(false);
    let mounted = Arc::new(AtomicBool::new(true));
    let cleanup = mounted.clone();
    on_cleanup(move || cleanup.store(false, Ordering::Relaxed));

    spawn_local(async move {
        while mounted.load(Ordering::Relaxed) {
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
                Ok(library) => movies.set(library.movies),
                Err(message) => error.set(Some(message)),
            }
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
        let token = csrf.get_value();
        spawn_local(async move {
            match Request::post("/api/v1/auth/logout")
                .header("x-csrf-token", &token)
                .send()
                .await
            {
                Ok(response) if matches!(response.status(), 204 | 401) => session.set(None),
                _ => {
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
            <button class="secondary" on:click=sign_out>"Sign out"</button>
        </div>
        <p class="eyebrow">"The beginning of your collection"</p>
        <h1>"Movie night starts here."</h1>
        <p class="notice">
            "Foundation preview: DVD conversion, playback, and ISO deletion are not enabled.
             Your original files are never modified."
        </p>
        <div class="toolbar">
            <h2>"Your library"</h2>
            <input aria-label="Search your library" placeholder="Search titles and descriptions"
                type="search" bind:value=query/>
        </div>
        {move || {
            let needle = query.get().to_lowercase();
            let filtered = movies.get().into_iter()
                .filter(|movie| format!("{} {}", movie.title, movie.description).to_lowercase().contains(&needle))
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                view! {
                    <section class="empty">
                        <h2>{if needle.is_empty() { "A home for your DVD collection" } else { "No matching movies" }}</h2>
                        <p class="subtle">
                            "No prepared movies yet. Import and playback will follow the media feasibility work."
                        </p>
                    </section>
                }.into_any()
            } else {
                view! {
                    <div class="grid">
                        {filtered.into_iter().map(|movie| view! {
                            <article class="movie">
                                <h2>{movie.title}</h2>
                                <small>{movie.year.map(|year| year.to_string())}</small>
                                <p>{movie.description}</p>
                            </article>
                        }).collect_view()}
                    </div>
                }.into_any()
            }
        }}
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
                        "Run read-only diagnostics"
                    </button>
                </div>
                <p class="subtle">"Checks installed DVD and OCR tools. No ISO is opened, converted, or deleted."</p>
                <For each={move || jobs.get().into_iter().take(4).collect::<Vec<_>>()}
                    key=|job| (job.id.clone(), job.state)
                    children=|job| {
                        let label = match job.state {
                            JobState::Queued => "Queued",
                            JobState::Running => "Checking media tools",
                            JobState::Succeeded => "Media prerequisites available",
                            JobState::Failed => "Media prerequisites need attention",
                        };
                        view! {
                            <article class="job">
                                <strong>{label}</strong>
                                {job.error.map(|message| view! { <p class="error">{message}</p> })}
                                {job.report.map(|report| report.tools.into_iter().map(|tool| view! {
                                    <div class="tool" class:available=tool.available>
                                        <strong>{tool.name}</strong>
                                        <span>{tool.detail}</span>
                                    </div>
                                }).collect_view())}
                            </article>
                        }
                    }/>
            </section>
        })}
    }
}
