use gloo_net::http::{Request, Response};
use leptos::{prelude::*, task::spawn_local};
use serde::de::DeserializeOwned;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use wetube_core::{
    ApiError, DiagnosticJob, IncomingFiles, JobState, Library, LoginRequest, Movie, ProbeJob,
    ProbeRequest, ProbeState, Role, Session, SystemStatus,
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
            <ImportsPanel csrf=csrf/>
        })}
    }
}

#[component]
fn ImportsPanel(csrf: StoredValue<String>) -> impl IntoView {
    let incoming = RwSignal::new(None::<IncomingFiles>);
    let jobs = RwSignal::new(Vec::<ProbeJob>::new());
    let selected = RwSignal::new(String::new());
    let title = RwSignal::new(String::new());
    let angle = RwSignal::new("1".to_owned());
    let copy_complete = RwSignal::new(false);
    let refreshing = RwSignal::new(false);
    let requesting = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
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
            let result = get::<IncomingFiles>("/api/v1/admin/incoming").await;
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
    spawn_local(async move {
        while mounted.load(Ordering::Relaxed) {
            let result = get::<Vec<ProbeJob>>("/api/v1/admin/probes").await;
            if !mounted.load(Ordering::Relaxed) {
                break;
            }
            match result {
                Ok(history) => jobs.set(history),
                Err(message) => error.set(Some(message)),
            }
            gloo_timers::future::TimeoutFuture::new(5000).await;
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
        let (Some(file), Ok(title), Ok(angle)) = (
            file,
            title.get_untracked().parse::<u8>(),
            angle.get_untracked().parse::<u8>(),
        ) else {
            error.set(Some("Select an ISO and a valid title/angle first.".into()));
            return;
        };
        let request = ProbeRequest {
            name: file.name,
            identity: file.identity,
            title,
            angle,
            copy_complete: copy_complete.get_untracked(),
        };
        let token = csrf.get_value();
        requesting.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                let response = Request::post("/api/v1/admin/probes")
                    .header("x-csrf-token", &token)
                    .json(&request)
                    .map_err(|error| error.to_string())?
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                decode::<ProbeJob>(response).await
            }
            .await;
            if requesting.try_update(|busy| *busy = false).is_none() {
                return;
            }
            copy_complete.set(false);
            match result {
                Ok(job) => jobs.update(|jobs| {
                    jobs.retain(|existing| existing.id != job.id);
                    jobs.insert(0, job);
                }),
                Err(message) => error.set(Some(message)),
            }
        });
    };

    view! {
        <section class="panel">
            <div class="toolbar">
                <h2>"Inspect an incoming DVD copy"</h2>
                <button class="secondary" on:click=move |_| refresh()
                    disabled=move || refreshing.get() || requesting.get()>
                    "Refresh incoming files"
                </button>
            </div>
            <p class="notice">
                "Inspection only: the worker keeps a private, checksum-verified copy.
                 Nothing is converted, published, or deleted. Title selection is manual."
            </p>
            {move || incoming.get().map(|list| if !list.configured {
                view! { <p class="subtle">"Incoming inspection is disabled. Configure the incoming root on the server and incoming/private scratch roots on the worker."</p> }.into_any()
            } else if list.files.is_empty() {
                view! { <p class="subtle">"No readable, non-empty ISO files in the incoming root. Finish copying, then refresh."</p> }.into_any()
            } else {
                view! { <p class="subtle">"Use finished copies only, never the original archive. Both processes must use the same incoming directory; jobs wait if the worker has no staging configuration."</p> }.into_any()
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
                <div class="probe-options">
                    <div>
                        <label for="dvd-title">"DVD title (1-99)"</label>
                        <input id="dvd-title" type="number" min="1" max="99" required bind:value=title/>
                    </div>
                    <div>
                        <label for="dvd-angle">"Angle (1-9)"</label>
                        <input id="dvd-angle" type="number" min="1" max="9" required bind:value=angle/>
                    </div>
                </div>
                <label class="confirmation">
                    <input type="checkbox" required prop:checked=move || copy_complete.get()
                        on:change=move |event| copy_complete.set(event_target_checked(&event))/>
                    "I confirm this copy is finished and its SMB writers are closed."
                </label>
                <p class="subtle">"Acknowledgement is not proof of writer exclusion and never authorizes source deletion."</p>
                <button type="submit" disabled=move || requesting.get() || refreshing.get()
                    || !copy_complete.get() || selected.get().is_empty()
                    || jobs.get().iter().any(|job| matches!(job.state, ProbeState::Queued | ProbeState::Staging | ProbeState::Probing))>
                    {move || if requesting.get() { "Queueing..." } else { "Stage and inspect selected title" }}
                </button>
            </form>
            {move || error.get().map(|message| view! { <p role="alert" class="error">{message}</p> })}
            <h2>"Inspection history"</h2>
            <For each=move || jobs.get() key=|job| (job.id.clone(), job.state)
                children=|job| {
                    let label = match job.state {
                        ProbeState::Queued => "Queued",
                        ProbeState::Staging => "Copying and verifying private input",
                        ProbeState::Probing => "Inspecting the selected title",
                        ProbeState::NeedsReview => "Needs review - not published",
                        ProbeState::Failed => "Failed - inputs retained",
                    };
                    view! {
                        <article class="job">
                            <h3 class="inspection-source">"Source: "{job.name.clone()}</h3>
                            <p class="inspection-state">{label}</p>
                            {inspection_summary(&job)}
                            {job.attempt_id.map(|attempt| view! { <p class="attempt">"Attempt: "{attempt}</p> })}
                            {job.error.map(|message| view! { <p class="error">{message}</p> })}
                            {job.report.map(|report| view! {
                                <details>
                                    <summary>"Raw staging / inspection JSON"</summary>
                                    <pre>{format!("{report:#}")}</pre>
                                </details>
                            })}
                        </article>
                    }
                }/>
            <p class="subtle">"All attempts, including failed/interrupted ones, count against scratch limits and require operator review. No automatic cleanup is enabled."</p>
        </section>
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
            <p class="subtle">"Only the selected title and angle are described. Nothing is published; this is not conversion or playback validation."</p>
            {metadata.is_none().then(|| view! {
                <p class="subtle">"DVD metadata is not available for this attempt. Staging details, if present, do not mean inspection succeeded."</p>
            })}
            <dl class="inspection-facts">
                <div><dt>"Approximate duration"</dt><dd>{approximate_duration(seconds)}</dd></div>
                <div><dt>"Chapter count"</dt><dd>{chapters}</dd></div>
            </dl>
            <p class="subtle">"Duration and chapter timing are approximate; full-disc preindexing is disabled."</p>
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
