mod http_updater;
pub use http_updater::Updater as HttpUpdater;

mod kafka_updater;
pub use kafka_updater::{write_offset, KafkaS3Config, Updater as KafkaUpdater};
