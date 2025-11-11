use anyhow::Context;
use axum::extract::Query;
use serde::{Deserialize, Serialize};
use std::time::Duration;

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
        let router = axum::Router::new()
            .route("/debug/pprof/allocs", axum::routing::get(memory_profile))
            .route("/debug/pprof/heap", axum::routing::get(memory_profile))
            .route("/debug/pprof/profile", axum::routing::get(cpu_profile));

        let listener = tokio::net::TcpListener::bind(self.address)
            .await
            .context("Failed to bind readiness server")?;

        axum::serve(listener, router.into_make_service())
            .await
            .context("Failed to serve readiness")
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

async fn memory_profile() -> Result<axum::body::Bytes, PprofError> {
    let prof_ctl = jemalloc_pprof::PROF_CTL.as_ref();

    match prof_ctl {
        None => Err(anyhow::anyhow!("heap profiling not activated").into()),
        Some(prof_ctl) => {
            let mut prof_ctl = prof_ctl.try_lock()?;

            if !prof_ctl.activated() {
                return Err(anyhow::anyhow!("heap profiling not activated").into());
            }

            let pprof = prof_ctl.dump_pprof().context("Failed to dump pprof")?;

            Ok(axum::body::Bytes::from(pprof))
        }
    }
}
