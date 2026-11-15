//! 读写模型调用的录像（设置 `recorder` 段产出的目录）。

pub use ::telemetry::recorder::blob::BlobId;
pub use ::telemetry::recorder::blob::BlobStore;
pub use ::telemetry::recorder::format::CallRecord;
pub use ::telemetry::recorder::format::Header;
pub use ::telemetry::recorder::format::Record;
pub use ::telemetry::recorder::format::RecordedParams;
pub use ::telemetry::recorder::format::FORMAT_VERSION;
pub use ::telemetry::recorder::reader;
pub use ::telemetry::recorder::writer::RecordingWriter;
