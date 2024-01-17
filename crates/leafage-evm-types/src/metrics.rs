pub use prometheus::{
    self, exponential_buckets, Counter, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec,
    IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Result,
};

/// Collect all the metrics for reporting.
pub fn gather() -> Vec<prometheus::proto::MetricFamily> {
    prometheus::gather()
}

/// Attempts to crate an `IntCounter`, returning `Err` if the registry does not accept the counter
/// (potentially due to naming conflict).
pub fn try_create_int_counter(name: &str, help: &str) -> Result<IntCounter> {
    let opts = Opts::new(name, help);
    let counter = IntCounter::with_opts(opts)?;
    prometheus::register(Box::new(counter.clone()))?;
    Ok(counter)
}

/// Attempts to crate an `IntCounterVec`, returning `Err` if the registry does not accept the counter
/// (potentially due to naming conflict).
pub fn try_create_int_counter_vec(
    name: &str,
    help: &str,
    labels: &[&str],
) -> Result<IntCounterVec> {
    let opts = Opts::new(name, help);
    let counter = IntCounterVec::new(opts, labels)?;
    prometheus::register(Box::new(counter.clone()))?;
    Ok(counter)
}

/// Attempts to crate an `Counter`, returning `Err` if the registry does not accept the counter
/// (potentially due to naming conflict).
pub fn try_create_counter(name: &str, help: &str) -> Result<Counter> {
    let opts = Opts::new(name, help);
    let counter = Counter::with_opts(opts)?;
    prometheus::register(Box::new(counter.clone()))?;
    Ok(counter)
}

/// Attempts to crate an `IntGauge`, returning `Err` if the registry does not accept the gauge
/// (potentially due to naming conflict).
pub fn try_create_int_gauge(name: &str, help: &str) -> Result<IntGauge> {
    let opts = Opts::new(name, help);
    let gauge = IntGauge::with_opts(opts)?;
    prometheus::register(Box::new(gauge.clone()))?;
    Ok(gauge)
}

/// Attempts to crate an `IntGaugeVec`, returning `Err` if the registry does not accept the gauge
/// (potentially due to naming conflict).
pub fn try_create_int_gauge_vec(name: &str, help: &str, labels: &[&str]) -> Result<IntGaugeVec> {
    let opts = Opts::new(name, help);
    let gauge = IntGaugeVec::new(opts, labels)?;
    prometheus::register(Box::new(gauge.clone()))?;
    Ok(gauge)
}

/// Attempts to crate an `Gauge`, returning `Err` if the registry does not accept the gauge
/// (potentially due to naming conflict).
pub fn try_create_gauge(name: &str, help: &str) -> Result<Gauge> {
    let opts = Opts::new(name, help);
    let gauge = Gauge::with_opts(opts)?;
    prometheus::register(Box::new(gauge.clone()))?;
    Ok(gauge)
}

/// Attempts to crate an `GaugeVec`, returning `Err` if the registry does not accept the gauge
/// (potentially due to naming conflict).
pub fn try_create_gauge_vec(name: &str, help: &str, labels: &[&str]) -> Result<GaugeVec> {
    let opts = Opts::new(name, help);
    let gauge = GaugeVec::new(opts, labels)?;
    prometheus::register(Box::new(gauge.clone()))?;
    Ok(gauge)
}

/// Attempts to crate a `Histogram`, returning `Err` if the registry does not accept the counter
/// (potentially due to naming conflict).
pub fn try_create_histogram(name: &str, help: &str) -> Result<Histogram> {
    let opts = HistogramOpts::new(name, help);
    let histogram = Histogram::with_opts(opts)?;
    prometheus::register(Box::new(histogram.clone()))?;
    Ok(histogram)
}

/// Attempts to crate a `Histogram`, returning `Err` if the registry does not accept the counter
/// (potentially due to naming conflict).
pub fn try_create_histogram_with_buckets(
    name: &str,
    help: &str,
    buckets: Vec<f64>,
) -> Result<Histogram> {
    let opts = HistogramOpts::new(name, help).buckets(buckets);
    let histogram = Histogram::with_opts(opts)?;
    prometheus::register(Box::new(histogram.clone()))?;
    Ok(histogram)
}

/// Attempts to create a `HistogramVector`, returning `Err` if the registry does not accept the counter
/// (potentially due to naming conflict).
pub fn try_create_histogram_vec(
    name: &str,
    help: &str,
    labels: &[&str],
    buckets: Option<Vec<f64>>,
) -> Result<HistogramVec> {
    let mut opts = HistogramOpts::new(name, help);
    if let Some(buckets) = buckets {
        opts = opts.buckets(buckets);
    }
    let histogram = HistogramVec::new(opts, labels)?;
    prometheus::register(Box::new(histogram.clone()))?;
    Ok(histogram)
}

pub fn processing_time_buckets() -> Vec<f64> {
    let mut buckets = vec![0.01, 0.025, 0.05, 0.1, 0.25, 0.5];
    buckets.extend_from_slice(&exponential_buckets(1.0, 1.3, 12).unwrap());
    buckets
}
