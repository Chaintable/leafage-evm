use anyhow::Context;
use axum::extract::Query;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::info;

struct PprofError(anyhow::Error);
impl<E> From<E> for PprofError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl axum::response::IntoResponse for PprofError {
    fn into_response(self) -> axum::response::Response {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            self.0.to_string(),
        )
            .into_response()
    }
}

const PPROF_DEFAULT_SECONDS: u64 = 30; // same as golang pprof
const PPROF_DEFAULT_SAMPLING: u64 = 99;

pub struct PProf {
    address: std::net::SocketAddr,
}

impl PProf {
    pub fn new(address: std::net::SocketAddr) -> Self {
        Self { address }
    }

    pub async fn start(self) -> anyhow::Result<()> {
        info!("starting pprof server at {}", self.address);
        let router = axum::Router::new()
            .route("/debug/pprof/allocs", axum::routing::get(memory_profile))
            .route(
                "/debug/pprof/allocs/activate",
                axum::routing::post(|| heap_profiling_route(true)),
            )
            .route(
                "/debug/pprof/allocs/deactivate",
                axum::routing::post(|| heap_profiling_route(false)),
            )
            .route("/debug/pprof/profile", axum::routing::get(cpu_profile));

        let listener = tokio::net::TcpListener::bind(self.address)
            .await
            .context("Failed to bind pprof server")?;

        axum::serve(listener, router.into_make_service())
            .await
            .context("Failed to serve pprof server")
            .map_err(Into::into)
    }
}

#[derive(Serialize, Deserialize)]
struct CpuProfileReq {
    seconds: Option<u64>,
    sampling: Option<u64>,
}

async fn cpu_profile(Query(req): Query<CpuProfileReq>) -> Result<axum::body::Bytes, PprofError> {
    use pprof::{protos::Message, ProfilerGuardBuilder};
    let profile_seconds = req.seconds.unwrap_or(PPROF_DEFAULT_SECONDS);
    let profile_sampling = req.sampling.unwrap_or(PPROF_DEFAULT_SAMPLING);

    let blocklist = &["libc", "libgcc", "pthread", "vdso"];
    let guard = ProfilerGuardBuilder::default()
        .frequency(profile_sampling.try_into()?)
        .blocklist(blocklist)
        .build()?;

    tokio::time::sleep(Duration::from_secs(profile_seconds.try_into()?)).await;

    let profile = guard.report().build()?.pprof()?;

    let mut content = Vec::new();
    profile.encode(&mut content)?;
    Ok(axum::body::Bytes::from(content))
}

/// Switches jemalloc heap-profile sampling on or off and returns whether it is
/// now on, or `None` when the process was started without `prof:true` in
/// `MALLOC_CONF` (sampling cannot be switched on then). Switching it off
/// also drops the samples taken so far.
pub async fn set_heap_profiling(active: bool) -> anyhow::Result<Option<bool>> {
    let Some(ctl) = jemalloc_pprof::PROF_CTL.as_ref() else {
        return Ok(None);
    };
    let mut ctl = ctl.lock().await;
    if active && !ctl.activated() {
        ctl.activate().context("activate jemalloc heap profiling")?;
    } else if !active && ctl.activated() {
        ctl.deactivate()
            .context("deactivate jemalloc heap profiling")?;
    }
    Ok(Some(ctl.activated()))
}

async fn heap_profiling_route(active: bool) -> Result<String, PprofError> {
    match set_heap_profiling(active).await? {
        Some(true) => Ok("heap profiling on\n".to_string()),
        Some(false) => Ok("heap profiling off\n".to_string()),
        None => {
            Err(anyhow::anyhow!("heap profiling unavailable: MALLOC_CONF lacks prof:true").into())
        }
    }
}

async fn memory_profile() -> Result<axum::body::Bytes, PprofError> {
    let mut prof_ctl = jemalloc_pprof::PROF_CTL
        .as_ref()
        .ok_or(anyhow::anyhow!("heap profiling not activated"))?
        .try_lock()?;

    require_profiling_activated(&prof_ctl)?;
    let pprof = prof_ctl.dump_pprof().context("Failed to dump pprof")?;
    Ok(axum::body::Bytes::from(pprof))
}

fn require_profiling_activated(
    prof_ctl: &jemalloc_pprof::JemallocProfCtl,
) -> Result<(), PprofError> {
    if prof_ctl.activated() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "heap profiling not activated (POST /debug/pprof/allocs/activate first)"
        )
        .into())
    }
}
