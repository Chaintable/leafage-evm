use leafage_evm_types::{
    exponential_buckets, try_create_histogram_vec, try_create_int_counter, try_create_int_gauge_vec,
};
use once_cell::sync::Lazy;
use prometheus::{HistogramVec, IntCounter, IntGaugeVec};

pub(crate) static DATABASE_OP_LATENCY_HIST: Lazy<HistogramVec> = Lazy::new(|| {
    try_create_histogram_vec(
        "leafage_database_op_latency_by_op_and_column",
        "Database operations latency by operation and column.",
        &["op", "column"],
        Some(exponential_buckets(0.00001, 1.3, 24).unwrap()),
    )
    .unwrap()
});

pub(crate) static BLOCK_PRODUCED_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter(
        "leafage_block_produced_total",
        "Total number of blocks produced since starting this node",
    )
    .unwrap()
});

pub(crate) static DATABASE_CACHE_USAGE: Lazy<IntGaugeVec> = Lazy::new(|| {
    try_create_int_gauge_vec(
        "leafage_database_cache_usage",
        "Database cache usage by column.",
        &["column"],
    )
    .unwrap()
});

pub(crate) static ACCOUNT_CACHE_HIT: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter("leafage_account_cache_hit", "Account cache hit count.").unwrap()
});

pub(crate) static ACCOUNT_CACHE_MISS: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter("leafage_account_cache_miss", "Account cache miss count.").unwrap()
});

pub(crate) static STORAGE_CACHE_HIT: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter("leafage_storage_cache_hit", "Storage cache hit count.").unwrap()
});

pub(crate) static STORAGE_CACHE_MISS: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter("leafage_storage_cache_miss", "Storage cache miss count.").unwrap()
});

pub(crate) static CODE_CACHE_HIT: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter("leafage_code_cache_hit", "Code cache hit count.").unwrap()
});

pub(crate) static CODE_CACHE_MISS: Lazy<IntCounter> = Lazy::new(|| {
    try_create_int_counter("leafage_code_cache_miss", "Code cache miss count.").unwrap()
});
